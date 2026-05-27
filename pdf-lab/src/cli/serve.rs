use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json},
    routing::get,
};
use clap::Args;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{Any, CorsLayer};

use pdf_core::{
    config::ClaudeConfig,
    schema::SchemaRegistry,
    search::{Backend, SearchResult, index::MetadataIndex, intent, keyword, merge, metadata, router, semantic, structural},
};

#[derive(Args)]
pub struct ServeArgs {
    #[arg(long, default_value = "7410", help = "Port to listen on")]
    pub port: u16,

    #[arg(long, help = "Override the outputs directory")]
    pub outputs_dir: Option<PathBuf>,
}

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    config: Arc<ClaudeConfig>,
    outputs_dir: Arc<std::sync::Mutex<PathBuf>>,
    schema_path: Arc<std::sync::Mutex<Option<PathBuf>>>,
}

// ── Request / response types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_top")]
    top: usize,
    /// Optional outputs_dir override from the UI (absolute path)
    outputs_dir: Option<String>,
}

fn default_top() -> usize { 12 }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSettingsBody {
    outputs_dir: Option<String>,
    schema_path: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsResponse {
    outputs_dir: String,
    api_key_set: bool,
    model: String,
    schema_path: Option<String>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    let config = &state.config;

    // UI may pass its own outputs_dir; otherwise use server default
    let outputs_dir: PathBuf = params
        .outputs_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| state.outputs_dir.lock().unwrap().clone());

    let schema = {
        let sp = state.schema_path.lock().unwrap().clone();
        SchemaRegistry::load_auto(sp.as_deref())
    };
    let person_field_names = schema.searchable_person_field_names();
    let date_field_names = schema.searchable_date_field_names();
    let index = match MetadataIndex::build(&outputs_dir, &person_field_names, &date_field_names) {
        Ok(idx) => idx,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    };

    let signals = intent::parse(&params.q, &index.known_persons, &schema.doc_type_values);
    let backends = router::route(&signals, &params.q, config, &index.known_persons, &schema.doc_type_values).await;

    let candidate_limit = params.top * 2;
    let mut all_results: Vec<SearchResult> = Vec::new();

    for backend in &backends {
        let mut results = match backend {
            Backend::Metadata => {
                let mut r = metadata::search(&signals, &index);
                r.truncate(candidate_limit);
                r
            }
            Backend::Keyword => {
                let scope = if backends.contains(&Backend::Metadata) {
                    let stems = metadata::matching_stems(&signals, &index);
                    if stems.is_empty() { None } else { Some(stems.into_iter().collect()) }
                } else {
                    None
                };
                let mut r = keyword::search(&signals, &outputs_dir, scope.as_ref());
                r.truncate(candidate_limit);
                r
            }
            Backend::Structural => structural::search(&signals, &outputs_dir),
            Backend::Semantic => {
                let mut r = semantic::search(&params.q);
                r.truncate(candidate_limit);
                r
            }
        };
        all_results.append(&mut results);
    }

    let combined = merge::merge(all_results, params.top);

    let json: Vec<serde_json::Value> = combined.iter().map(|r| {
        // Only include source_path if the file actually exists on disk; stale index
        // entries whose original PDF was moved/deleted would otherwise cause the
        // preview panel to render a "file not found" error from the server.
        let source_path_str = r.source_path.as_ref()
            .filter(|p| p.exists())
            .map(|p| p.display().to_string());
        let file_name = r.source_path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| r.file_name.clone());
        json!({
            "filePath": r.file_path.display().to_string(),
            "fileName": file_name,
            "sourcePath": source_path_str,
            "snippet": r.snippet,
            "pageNum": r.page_num,
            "backend": r.backend.to_string(),
            "score": r.score,
            "meta": {
                "person": r.meta.person,
                "docType": r.meta.doc_type,
                "date": r.meta.date,
                "pages": r.meta.pages,
                "words": r.meta.words,
                "keyword": r.meta.keyword,
            }
        })
    }).collect();

    Json(json).into_response()
}

async fn handle_get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let outputs_dir = state.outputs_dir.lock().unwrap().display().to_string();
    let schema_path = state.schema_path.lock().unwrap()
        .as_ref()
        .map(|p| p.display().to_string());
    Json(SettingsResponse {
        outputs_dir,
        api_key_set: !state.config.api_key.is_empty(),
        model: state.config.model.clone(),
        schema_path,
    })
}

async fn handle_save_settings(
    State(state): State<AppState>,
    Json(body): Json<SaveSettingsBody>,
) -> impl IntoResponse {
    let mut config = (*state.config).clone();
    if let Some(dir) = body.outputs_dir {
        *state.outputs_dir.lock().unwrap() = PathBuf::from(&dir);
        config.outputs_dir = Some(dir);
    }
    if let Some(sp) = body.schema_path {
        *state.schema_path.lock().unwrap() = Some(PathBuf::from(&sp));
        config.schema_path = Some(sp);
    }
    let _ = config.save();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

async fn handle_file(Query(params): Query<FileQuery>) -> impl IntoResponse {
    let path = PathBuf::from(&params.path);
    if !path.is_absolute() {
        return (StatusCode::BAD_REQUEST, "path must be absolute").into_response();
    }
    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("md") | Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
            (StatusCode::OK, headers, bytes).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "file not found").into_response(),
    }
}

async fn handle_index_status(State(state): State<AppState>) -> impl IntoResponse {
    let outputs_dir = state.outputs_dir.lock().unwrap().clone();
    let schema = {
        let sp = state.schema_path.lock().unwrap().clone();
        SchemaRegistry::load_auto(sp.as_deref())
    };
    let person_field_names = schema.searchable_person_field_names();
    let date_field_names = schema.searchable_date_field_names();
    let (files_indexed, size_bytes) = match MetadataIndex::build(&outputs_dir, &person_field_names, &date_field_names) {
        Ok(idx) => {
            let count = idx.entries.len();
            let size: u64 = idx.entries.values()
                .filter_map(|e| std::fs::metadata(&e.file_path).ok())
                .map(|m| m.len())
                .sum();
            (count, size)
        }
        Err(_) => (0, 0),
    };
    Json(json!({
        "filesIndexed": files_indexed,
        "totalFiles": files_indexed,
        "sizeBytes": size_bytes,
        "lastSyncedAt": null,
        "running": false,
        "outputsDir": outputs_dir.display().to_string(),
    }))
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    let outputs_dir = config.resolve_outputs_dir(args.outputs_dir);
    let schema_path = config.schema_path.as_deref().map(PathBuf::from);

    let state = AppState {
        config: Arc::new(config),
        outputs_dir: Arc::new(std::sync::Mutex::new(outputs_dir.clone())),
        schema_path: Arc::new(std::sync::Mutex::new(schema_path)),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/search", get(handle_search))
        .route("/settings", get(handle_get_settings))
        .route("/settings", axum::routing::post(handle_save_settings))
        .route("/index/status", get(handle_index_status))
        .route("/file", get(handle_file))
        .with_state(state)
        .layer(cors);

    let addr = format!("127.0.0.1:{}", args.port);
    println!("pdf-lab serve → http://{addr}  (outputs: {})", outputs_dir.display());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
