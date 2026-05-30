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
    search::{SearchMode, execute, index::MetadataIndex, intent::IntentIndex, merged_dirs},
};

#[derive(Args)]
pub struct ServeArgs {
    #[arg(long, default_value = "7410", help = "Port to listen on")]
    pub port: u16,

    #[arg(long, help = "Override the index directory")]
    pub index_dir: Option<PathBuf>,
}

// ── Shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    config: Arc<ClaudeConfig>,
    index_dir: Arc<std::sync::Mutex<PathBuf>>,
    schema_path: Arc<std::sync::Mutex<Option<PathBuf>>>,
    intent_index: Arc<tokio::sync::RwLock<IntentIndex>>,
    doc_type_values: Arc<tokio::sync::RwLock<Vec<String>>>,
}

// ── Request / response types ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    #[serde(default = "default_top")]
    top: usize,
    index_dir: Option<String>,
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_top() -> usize { 12 }
fn default_mode() -> String { "text".to_string() }

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveSettingsBody {
    index_dir: Option<String>,
    schema_path: Option<String>,
    mode: Option<String>,
    api_key: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsResponse {
    index_dir: String,
    api_key_set: bool,
    model: String,
    schema_path: Option<String>,
    mode: String,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn handle_search(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    let index_base: PathBuf = params
        .index_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| state.index_dir.lock().unwrap().clone());

    let mode: SearchMode = match params.mode.parse() {
        Ok(m) => m,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid mode; expected \"text\" or \"images\""}))).into_response(),
    };

    let schema = {
        let sp = state.schema_path.lock().unwrap().clone();
        match SchemaRegistry::load_auto(sp.as_deref()) {
            Ok(s) => s,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        }
    };

    let intent_index = state.intent_index.read().await;
    let combined = execute(&params.q, &index_base, &mode, params.top, &intent_index, &state.config, &schema).await;

    let json: Vec<serde_json::Value> = combined.iter().map(|r| r.to_json()).collect();
    Json(json).into_response()
}

async fn handle_get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let index_dir = state.index_dir.lock().unwrap().display().to_string();
    let schema_path = state.schema_path.lock().unwrap()
        .as_ref()
        .map(|p| p.display().to_string());
    Json(SettingsResponse {
        index_dir,
        api_key_set: !state.config.api_key.is_empty(),
        model: state.config.model.clone(),
        schema_path,
        mode: state.config.mode.clone().unwrap_or_else(|| "offline".to_string()),
    })
}

async fn handle_save_settings(
    State(state): State<AppState>,
    Json(body): Json<SaveSettingsBody>,
) -> impl IntoResponse {
    let mut config = (*state.config).clone();
    if let Some(dir) = body.index_dir {
        *state.index_dir.lock().unwrap() = PathBuf::from(&dir);
        config.index_dir = Some(dir);
    }
    if let Some(sp) = body.schema_path {
        *state.schema_path.lock().unwrap() = Some(PathBuf::from(&sp));
        config.schema_path = Some(sp.clone());
        // Rebuild intent index when schema changes
        if let Ok(new_schema) = SchemaRegistry::load_auto(Some(std::path::Path::new(&sp))) {
            if let Ok(new_index) = IntentIndex::new(&new_schema.doc_type_values) {
                *state.intent_index.write().await = new_index;
                *state.doc_type_values.write().await = new_schema.doc_type_values;
            }
        }
    }
    if let Some(m) = body.mode {
        config.mode = Some(m);
    }
    if let Some(k) = body.api_key {
        if !k.is_empty() {
            config.api_key = k;
        }
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
    let index_base = state.index_dir.lock().unwrap().clone();
    let schema = {
        let sp = state.schema_path.lock().unwrap().clone();
        match SchemaRegistry::load_auto(sp.as_deref()) {
            Ok(s) => s,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
        }
    };
    let person_field_names = schema.searchable_person_field_names();
    let date_field_names = schema.searchable_date_field_names();

    let mut files_indexed = 0usize;
    let mut size_bytes = 0u64;
    for mode in [SearchMode::Text, SearchMode::Images] {
        let dirs = merged_dirs(&index_base, &mode);
        let idx = MetadataIndex::build_merged_with_fields(
            &dirs.offline, &dirs.online, &person_field_names, &date_field_names,
        ).unwrap_or_else(|_| MetadataIndex { entries: Default::default(), known_persons: vec![] });
        files_indexed += idx.entries.len();
        size_bytes += idx.entries.values()
            .filter_map(|e| std::fs::metadata(&e.file_path).ok())
            .map(|m| m.len())
            .sum::<u64>();
    }

    Json(json!({
        "filesIndexed": files_indexed,
        "totalFiles": files_indexed,
        "sizeBytes": size_bytes,
        "lastSyncedAt": null,
        "running": false,
        "indexDir": index_base.display().to_string(),
    })).into_response()
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let config = ClaudeConfig::load()?;
    let index_dir = config.resolve_index_dir(args.index_dir);
    let schema_path = config.schema_path.as_deref().map(PathBuf::from);

    let initial_schema = SchemaRegistry::load_auto(schema_path.as_deref())?;
    let intent_index = Arc::new(tokio::sync::RwLock::new(
        IntentIndex::new(&initial_schema.doc_type_values)?
    ));
    let doc_type_values = Arc::new(tokio::sync::RwLock::new(
        initial_schema.doc_type_values.clone()
    ));

    let state = AppState {
        config: Arc::new(config),
        index_dir: Arc::new(std::sync::Mutex::new(index_dir.clone())),
        schema_path: Arc::new(std::sync::Mutex::new(schema_path)),
        intent_index,
        doc_type_values,
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
    println!("pdf-lab serve → http://{addr}  (index: {})", index_dir.display());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
