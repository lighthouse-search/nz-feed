use axum::{
    extract::Query,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tower_http::cors::CorsLayer;

#[derive(Debug, Deserialize)]
struct FiltersConfig {
    inclusion_terms: Vec<String>,
    exclusion_terms: Vec<String>,
    #[serde(default)]
    require_list_membership: bool,
    list_uri: Option<String>,
}

// A compiled filter term with its original string for display
struct FilterTerm {
    regex: Regex,
    original: String,
}

struct CompiledFilters {
    inclusion_terms: Vec<FilterTerm>,
    exclusion_terms: Vec<FilterTerm>,
    require_list_membership: bool,
    list_uri: Option<String>,
}

// Bluesky API response structures for list members
#[derive(Debug, Deserialize)]
struct ListResponse {
    items: Vec<ListItem>,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListItem {
    subject: ListSubject,
}

#[derive(Debug, Deserialize)]
struct ListSubject {
    did: String,
}

// Shared state for list members (can be refreshed periodically)
struct ListMembers {
    members: HashSet<String>,
    enabled: bool,
}

// Jetstream message structures
#[derive(Debug, Deserialize)]
struct JetstreamMessage {
    kind: String,
    did: Option<String>,
    time_us: Option<u64>,
    commit: Option<CommitData>,
}

#[derive(Debug, Deserialize)]
struct CommitData {
    collection: String,
    rkey: String,
    operation: String,
    record: Option<Post>,
    cid: Option<String>,
    rev: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Post {
    #[serde(rename = "$type")]
    record_type: Option<String>,
    text: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    langs: Option<Vec<String>>,
}

const JETSTREAM_URL: &str = "wss://jetstream2.us-east.bsky.network/subscribe";

// AT Protocol Feed Response structures
#[derive(Debug, Serialize)]
struct FeedResponse {
    feed: Vec<FeedItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct FeedItem {
    post: String, // AT-URI format: at://did:plc:xxxxx/app.bsky.feed.post/xxxxx
}

#[derive(Debug, Serialize)]
struct DescribeFeedGeneratorResponse {
    did: String,
    feeds: Vec<FeedDescription>,
}

#[derive(Debug, Serialize)]
struct FeedDescription {
    uri: String, // at://did:plc:xxxxx/app.bsky.feed.generator/xxxxx
    #[serde(skip_serializing_if = "Option::is_none")]
    cid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GetFeedSkeletonQuery {
    feed: String,
    #[serde(default = "default_limit")]
    limit: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

fn default_limit() -> u32 {
    50
}

// Handler for DID document - identifies your feed generator service
async fn handle_did_document() -> impl IntoResponse {
    let did_doc = serde_json::json!({
        "@context": ["https://www.w3.org/ns/did/v1"],
        "id": "did:web:seer.oracularhades.com",  // Replace with your actual domain
        "service": [{
            "id": "#bsky_fg",
            "type": "BskyFeedGenerator",
            "serviceEndpoint": "https://seer.oracularhades.com"  // Replace with your actual endpoint
        }]
    });

    Json(did_doc)
}

// Handler for describing the feed generator
async fn describe_feed_generator() -> impl IntoResponse {
    let response = DescribeFeedGeneratorResponse {
        did: "did:web:seer.oracularhades.com".to_string(),  // Replace with your actual DID
        feeds: vec![
            FeedDescription {
                uri: "at://did:web:seer.oracularhades.com/app.bsky.feed.generator/nzseertest".to_string(),
                cid: None,
            }
        ],
    };

    Json(response)
}

// Handler for getting the feed skeleton
async fn get_feed_skeleton(Query(_params): Query<GetFeedSkeletonQuery>) -> impl IntoResponse {
    // This is where you'd query your database/Redis for posts that match your feed criteria
    // For now, returning an empty feed as an example

    let response = FeedResponse {
        feed: vec![
            // Example post - replace with actual posts from your database
            // FeedItem {
            //     post: "at://did:plc:xxxxx/app.bsky.feed.post/xxxxx".to_string(),
            // }
        ],
        cursor: None,
    };

    (StatusCode::OK, Json(response))
}

fn parse_filter_term(term: &str) -> Result<FilterTerm, Box<dyn std::error::Error>> {
    let regex = if term.starts_with('/') && term.ends_with('/') && term.len() > 2 {
        // Explicit regex: user controls the pattern
        let pattern = &term[1..term.len() - 1];
        Regex::new(&format!("(?i){}", pattern))?
    } else {
        // Plain terms: escape regex special chars and add word boundaries
        // This ensures "Ward" matches "Ward" but not "toward" or "reward"
        let escaped = regex::escape(term);
        Regex::new(&format!("(?i)\\b{}\\b", escaped))?
    };

    Ok(FilterTerm {
        regex,
        original: term.to_string(),
    })
}

fn load_filters() -> Result<CompiledFilters, Box<dyn std::error::Error>> {
    let filters_path = "filters.json";
    let filters_content = fs::read_to_string(filters_path)?;
    let config: FiltersConfig = serde_json::from_str(&filters_content)?;

    let mut inclusion_terms = Vec::new();
    let mut exclusion_terms = Vec::new();

    for term in &config.inclusion_terms {
        match parse_filter_term(term) {
            Ok(filter_term) => inclusion_terms.push(filter_term),
            Err(e) => eprintln!("Warning: Failed to parse inclusion term '{}': {}", term, e),
        }
    }

    for term in &config.exclusion_terms {
        match parse_filter_term(term) {
            Ok(filter_term) => exclusion_terms.push(filter_term),
            Err(e) => eprintln!("Warning: Failed to parse exclusion term '{}': {}", term, e),
        }
    }

    Ok(CompiledFilters {
        inclusion_terms,
        exclusion_terms,
        require_list_membership: config.require_list_membership,
        list_uri: config.list_uri,
    })
}

async fn fetch_list_members(client: &Client, list_uri: &str) -> Result<HashSet<String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut members = HashSet::new();
    let mut cursor: Option<String> = None;

    println!("Fetching list members from: {}", list_uri);

    loop {
        let mut url = format!(
            "https://public.api.bsky.app/xrpc/app.bsky.graph.getList?list={}",
            urlencoding::encode(list_uri)
        );

        if let Some(ref c) = cursor {
            url.push_str(&format!("&cursor={}", urlencoding::encode(c)));
        }

        let response = client.get(&url).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("Failed to fetch list: {} - {}", status, body).into());
        }

        let list_response: ListResponse = response.json().await?;

        for item in list_response.items {
            members.insert(item.subject.did);
        }

        match list_response.cursor {
            Some(c) if !c.is_empty() => cursor = Some(c),
            _ => break,
        }
    }

    println!("Loaded {} members from list", members.len());
    Ok(members)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load and compile filters from file
    let filters = Arc::new(load_filters()?);
    println!(
        "Loaded {} inclusion terms and {} exclusion terms from filters.json",
        filters.inclusion_terms.len(),
        filters.exclusion_terms.len()
    );

    // Load list members if required
    let list_members = if filters.require_list_membership {
        if let Some(ref list_uri) = filters.list_uri {
            let client = Client::new();
            match fetch_list_members(&client, list_uri).await {
                Ok(members) => Arc::new(RwLock::new(ListMembers {
                    members,
                    enabled: true,
                })),
                Err(e) => {
                    eprintln!("Warning: Failed to fetch list members: {}", e);
                    eprintln!("Continuing without list filtering...");
                    Arc::new(RwLock::new(ListMembers {
                        members: HashSet::new(),
                        enabled: false,
                    }))
                }
            }
        } else {
            eprintln!("Warning: require_list_membership is true but no list_uri provided");
            Arc::new(RwLock::new(ListMembers {
                members: HashSet::new(),
                enabled: false,
            }))
        }
    } else {
        Arc::new(RwLock::new(ListMembers {
            members: HashSet::new(),
            enabled: false,
        }))
    };

    // Build the Axum router
    let app = Router::new()
        .route("/.well-known/did.json", get(handle_did_document))
        .route("/xrpc/app.bsky.feed.describeFeedGenerator", get(describe_feed_generator))
        .route("/xrpc/app.bsky.feed.getFeedSkeleton", get(get_feed_skeleton))
        .layer(CorsLayer::permissive());

    // Start the HTTP server
    let addr = SocketAddr::from(([0, 0, 0, 0], 3001));
    println!("Starting feed generator server on {}", addr);

    let server = axum::serve(
        tokio::net::TcpListener::bind(addr).await?,
        app
    );

    // Start the Jetstream WebSocket listener
    let filters_clone = Arc::clone(&filters);
    let list_members_clone = Arc::clone(&list_members);
    let jetstream_task = tokio::spawn(async move {
        if let Err(e) = run_jetstream_listener(filters_clone, list_members_clone).await {
            eprintln!("Jetstream listener error: {}", e);
        }
    });

    // Run both tasks concurrently
    tokio::select! {
        result = server => {
            result?;
        }
        _ = jetstream_task => {
            println!("Jetstream task completed");
        }
    }

    Ok(())
}

async fn run_jetstream_listener(
    filters: Arc<CompiledFilters>,
    list_members: Arc<RwLock<ListMembers>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("Connecting to Bluesky Jetstream...");

    // Connect to Jetstream WebSocket
    let url = format!("{}?wantedCollections=app.bsky.feed.post", JETSTREAM_URL);
    let (ws_stream, _) = connect_async(&url).await?;
    println!("Connected to Jetstream!");
    println!(
        "Filtering posts with {} inclusion and {} exclusion terms",
        filters.inclusion_terms.len(),
        filters.exclusion_terms.len()
    );

    {
        let members = list_members.read().await;
        if members.enabled {
            println!("List membership filter: ENABLED ({} members)", members.members.len());
        } else {
            println!("List membership filter: DISABLED");
        }
    }
    println!("---");

    let (mut _write, mut read) = ws_stream.split();

    // Listen for messages
    while let Some(message) = read.next().await {
        match message {
            Ok(Message::Text(text)) => {
                if let Err(e) = process_message(&text, &filters, &list_members).await {
                    eprintln!("Error processing message: {}", e);
                }
            }
            Ok(Message::Close(_)) => {
                println!("Connection closed by server");
                break;
            }
            Err(e) => {
                eprintln!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }

    Ok(())
}

fn term_matches(text: &str, term: &FilterTerm) -> bool {
    term.regex.is_match(text)
}

// ANSI color codes
const HIGHLIGHT_START: &str = "\x1b[1;33m"; // Bold yellow
const HIGHLIGHT_END: &str = "\x1b[0m";      // Reset

fn highlight_matches(text: &str, term: &FilterTerm) -> String {
    let mut result = String::new();
    let mut last_end = 0;

    for mat in term.regex.find_iter(text) {
        result.push_str(&text[last_end..mat.start()]);
        result.push_str(HIGHLIGHT_START);
        result.push_str(mat.as_str());
        result.push_str(HIGHLIGHT_END);
        last_end = mat.end();
    }
    result.push_str(&text[last_end..]);
    result
}

struct MatchResult {
    term_display: String,
    highlighted_text: String,
}

fn matches_filter(text: &str, filters: &CompiledFilters) -> Option<MatchResult> {
    // Check if any exclusion term is present
    for term in &filters.exclusion_terms {
        if term_matches(text, term) {
            return None; // Excluded
        }
    }

    // Check if any inclusion term is present
    for term in &filters.inclusion_terms {
        if term_matches(text, term) {
            return Some(MatchResult {
                term_display: term.original.clone(),
                highlighted_text: highlight_matches(text, term),
            });
        }
    }

    None
}

async fn process_message(
    text: &str,
    filters: &CompiledFilters,
    list_members: &Arc<RwLock<ListMembers>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let msg: JetstreamMessage = serde_json::from_str(text)?;

    // Only process commit messages
    if msg.kind == "commit" {
        if let Some(commit) = &msg.commit {
            // Check if this is a post in the feed
            if commit.collection == "app.bsky.feed.post" {
                if let Some(record) = &commit.record {
                    if let Some(post_text) = &record.text {
                        if let Some(did) = &msg.did {
                            // Check list membership if enabled
                            {
                                let members = list_members.read().await;
                                if members.enabled && !members.members.contains(did) {
                                    return Ok(()); // Author not on list, skip
                                }
                            }

                            // Apply filters - only process posts that match
                            let match_result = match matches_filter(post_text, filters) {
                                Some(result) => result,
                                None => return Ok(()),
                            };

                            // Construct the AT-URI for this post
                            let at_uri = format!("at://{}/{}/{}", did, commit.collection, commit.rkey);

                            // Log the post
                            println!("Post from {}", did);
                            println!("   URI: {}", at_uri);
                            println!("   Matched: {}", match_result.term_display);
                            println!("   Text: {}", match_result.highlighted_text);

                            if let Some(created_at) = &record.created_at {
                                println!("   Created: {}", created_at);
                            }

                            if let Some(langs) = &record.langs {
                                println!("   Languages: {:?}", langs);
                            }

                            // TODO: Parse embed data to detect images, gifs, videos, and quote posts
                            println!("---");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
