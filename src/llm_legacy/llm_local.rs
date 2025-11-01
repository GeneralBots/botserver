use actix_web::{post, web, HttpRequest, HttpResponse, Result};
use dotenvy::dotenv;
use log::{error, info};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use tokio::time::{sleep, Duration};

use crate::config::ConfigManager;
use crate::shared::models::schema::bots::dsl::*;
use crate::shared::state::AppState;
use diesel::prelude::*;

// OpenAI-compatible request/response structures
#[derive(Debug, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChatCompletionResponse {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<Choice>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Choice {
    message: ChatMessage,
    finish_reason: String,
}

// Llama.cpp server request/response structures
#[derive(Debug, Serialize, Deserialize)]
struct LlamaCppRequest {
    prompt: String,
    n_predict: Option<i32>,
    temperature: Option<f32>,
    top_k: Option<i32>,
    top_p: Option<f32>,
    stream: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LlamaCppResponse {
    content: String,
    stop: bool,
    generation_settings: Option<serde_json::Value>,
}

pub async fn ensure_llama_servers_running(
    app_state: &AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = app_state.conn.clone();
    let config_manager = ConfigManager::new(conn.clone());

    // Get default bot ID from database
    let default_bot_id = {
        let mut conn = conn.lock().unwrap();
        bots.filter(name.eq("default"))
            .select(id)
            .first::<uuid::Uuid>(&mut *conn)
            .unwrap_or_else(|_| uuid::Uuid::nil())
    };

    // Get configuration from config using default bot ID
    let llm_url = config_manager.get_config(&default_bot_id, "llm-url", None)?;
    let llm_model = config_manager.get_config(&default_bot_id, "llm-model", None)?;

    let embedding_url = config_manager.get_config(&default_bot_id, "embedding-url", None)?;
    let embedding_model = config_manager.get_config(&default_bot_id, "embedding-model", None)?;

    let llm_server_path = config_manager.get_config(&default_bot_id, "llm-server-path", None)?;

    info!(" Starting LLM servers...");
    info!(" Configuration:");
    info!("   LLM URL: {}", llm_url);
    info!("   Embedding URL: {}", embedding_url);
    info!("   LLM Model: {}", llm_model);
    info!("   Embedding Model: {}", embedding_model);
    info!("   LLM Server Path: {}", llm_server_path);

    // Check if servers are already running
    let llm_running = is_server_running(&llm_url).await;
    let embedding_running = is_server_running(&embedding_url).await;

    if llm_running && embedding_running {
        info!("✅ Both LLM and Embedding servers are already running");
        return Ok(());
    }

    // Start servers that aren't running
    let mut tasks = vec![];

    if !llm_running && !llm_model.is_empty() {
        info!("🔄 Starting LLM server...");
        tasks.push(tokio::spawn(start_llm_server(
            llm_server_path.clone(),
            llm_model.clone(),
            llm_url.clone(),
        )));
    } else if llm_model.is_empty() {
        info!("⚠️  LLM_MODEL not set, skipping LLM server");
    }

    if !embedding_running && !embedding_model.is_empty() {
        info!("🔄 Starting Embedding server...");
        tasks.push(tokio::spawn(start_embedding_server(
            llm_server_path.clone(),
            embedding_model.clone(),
            embedding_url.clone(),
        )));
    } else if embedding_model.is_empty() {
        info!("⚠️  EMBEDDING_MODEL not set, skipping Embedding server");
    }

    // Wait for all server startup tasks
    for task in tasks {
        task.await??;
    }

    // Wait for servers to be ready with verbose logging
    info!("⏳ Waiting for servers to become ready...");

    let mut llm_ready = llm_running || llm_model.is_empty();
    let mut embedding_ready = embedding_running || embedding_model.is_empty();

    let mut attempts = 0;
    let max_attempts = 60; // 2 minutes total

    while attempts < max_attempts && (!llm_ready || !embedding_ready) {
        sleep(Duration::from_secs(2)).await;

        info!(
            "🔍 Checking server health (attempt {}/{})...",
            attempts + 1,
            max_attempts
        );

        if !llm_ready && !llm_model.is_empty() {
            if is_server_running(&llm_url).await {
                info!("   ✅ LLM server ready at {}", llm_url);
                llm_ready = true;
            } else {
                info!("   ❌ LLM server not ready yet");
            }
        }

        if !embedding_ready && !embedding_model.is_empty() {
            if is_server_running(&embedding_url).await {
                info!("   ✅ Embedding server ready at {}", embedding_url);
                embedding_ready = true;
            } else {
                info!("   ❌ Embedding server not ready yet");
            }
        }

        attempts += 1;

        if attempts % 10 == 0 {
            info!(
                "⏰ Still waiting for servers... (attempt {}/{})",
                attempts, max_attempts
            );
        }
    }

    if llm_ready && embedding_ready {
        info!("🎉 All llama.cpp servers are ready and responding!");
        Ok(())
    } else {
        let mut error_msg = "❌ Servers failed to start within timeout:".to_string();
        if !llm_ready && !llm_model.is_empty() {
            error_msg.push_str(&format!("\n   - LLM server at {}", llm_url));
        }
        if !embedding_ready && !embedding_model.is_empty() {
            error_msg.push_str(&format!("\n   - Embedding server at {}", embedding_url));
        }
        Err(error_msg.into())
    }
}

async fn start_llm_server(
    llama_cpp_path: String,
    model_path: String,
    url: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = url.split(':').last().unwrap_or("8081");

    std::env::set_var("OMP_NUM_THREADS", "20");
    std::env::set_var("OMP_PLACES", "cores");
    std::env::set_var("OMP_PROC_BIND", "close");

    // "cd {} && numactl --interleave=all ./llama-server -m {} --host 0.0.0.0 --port {} --threads 20 --threads-batch 40 --temp 0.7 --parallel 1 --repeat-penalty 1.1 --ctx-size 8192 --batch-size 8192 -n 4096 --mlock --no-mmap --flash-attn  --no-kv-offload  --no-mmap &",
    // Read config values with defaults
    let n_moe = env::var("LLM_SERVER_N_MOE").unwrap_or("4".to_string());
    let ctx_size = env::var("LLM_SERVER_CTX_SIZE").unwrap_or("4096".to_string());
    let parallel = env::var("LLM_SERVER_PARALLEL").unwrap_or("1".to_string());
    let cont_batching = env::var("LLM_SERVER_CONT_BATCHING").unwrap_or("true".to_string());
    let mlock = env::var("LLM_SERVER_MLOCK").unwrap_or("true".to_string());
    let no_mmap = env::var("LLM_SERVER_NO_MMAP").unwrap_or("true".to_string());
    let gpu_layers = env::var("LLM_SERVER_GPU_LAYERS").unwrap_or("20".to_string());

    // Build command arguments dynamically
    let mut args = format!(
        "-m {} --host 0.0.0.0 --port {} --top_p 0.95 --temp 0.6 --ctx-size {} --repeat-penalty 1.2 -ngl {}",
        model_path, port, ctx_size, gpu_layers
    );

    if n_moe != "0" {
        args.push_str(&format!(" --n-cpu-moe {}", n_moe));
    }
    if parallel != "1" {
        args.push_str(&format!(" --parallel {}", parallel));
    }
    if cont_batching == "true" {
        args.push_str(" --cont-batching");
    }
    if mlock == "true" {
        args.push_str(" --mlock");
    }
    if no_mmap == "true" {
        args.push_str(" --no-mmap");
    }

    if cfg!(windows) {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/C").arg(format!(
            "cd {} && .\\llama-server.exe {}",
            llama_cpp_path, args
        ));
        cmd.spawn()?;
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(format!(
            "cd {} && ./llama-server {} &",
            llama_cpp_path, args
        ));
        cmd.spawn()?;
    }

    Ok(())
}

async fn start_embedding_server(
    llama_cpp_path: String,
    model_path: String,
    url: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let port = url.split(':').last().unwrap_or("8082");

    if cfg!(windows) {
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/c").arg(format!(
            "cd {} && .\\llama-server.exe -m {} --host 0.0.0.0 --port {} --embedding --n-gpu-layers 99",
            llama_cpp_path, model_path, port
        ));
        cmd.spawn()?;
    } else {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(format!(
            "cd {} && ./llama-server -m {} --host 0.0.0.0 --port {} --embedding --n-gpu-layers 99 &",
            llama_cpp_path, model_path, port
        ));
        cmd.spawn()?;
    }

    Ok(())
}

async fn is_server_running(url: &str) -> bool {
    
    
    let client = reqwest::Client::new();
    match client.get(&format!("{}/health", url)).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

// Convert OpenAI chat messages to a single prompt
fn messages_to_prompt(messages: &[ChatMessage]) -> String {
    let mut prompt = String::new();

    for message in messages {
        match message.role.as_str() {
            "system" => {
                prompt.push_str(&format!("System: {}\n\n", message.content));
            }
            "user" => {
                prompt.push_str(&format!("User: {}\n\n", message.content));
            }
            "assistant" => {
                prompt.push_str(&format!("Assistant: {}\n\n", message.content));
            }
            _ => {
                prompt.push_str(&format!("{}: {}\n\n", message.role, message.content));
            }
        }
    }

    prompt.push_str("Assistant: ");
    prompt
}

// Proxy endpoint
#[post("/local/v1/chat/completions")]
pub async fn chat_completions_local(
    req_body: web::Json<ChatCompletionRequest>,
    _req: HttpRequest,
) -> Result<HttpResponse> {
    dotenv().ok().unwrap();

    // Get llama.cpp server URL
    let llama_url = env::var("LLM_URL").unwrap_or_else(|_| "http://localhost:8081".to_string());

    // Convert OpenAI format to llama.cpp format
    let prompt = messages_to_prompt(&req_body.messages);

    let llama_request = LlamaCppRequest {
        prompt,
        n_predict: Some(500), // Adjust as needed
        temperature: Some(0.7),
        top_k: Some(40),
        top_p: Some(0.9),
        stream: req_body.stream,
    };

    // Send request to llama.cpp server
    let client = Client::builder()
        .timeout(Duration::from_secs(500)) // 2 minute timeout
        .build()
        .map_err(|e| {
            error!("Error creating HTTP client: {}", e);
            actix_web::error::ErrorInternalServerError("Failed to create HTTP client")
        })?;

    let response = client
        .post(&format!("{}/v1/completion", llama_url))
        .header("Content-Type", "application/json")
        .json(&llama_request)
        .send()
        .await
        .map_err(|e| {
            error!("Error calling llama.cpp server: {}", e);
            actix_web::error::ErrorInternalServerError("Failed to call llama.cpp server")
        })?;

    let status = response.status();

    if status.is_success() {
        let llama_response: LlamaCppResponse = response.json().await.map_err(|e| {
            error!("Error parsing llama.cpp response: {}", e);
            actix_web::error::ErrorInternalServerError("Failed to parse llama.cpp response")
        })?;

        // Convert llama.cpp response to OpenAI format
        let openai_response = ChatCompletionResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            model: req_body.model.clone(),
            choices: vec![Choice {
                message: ChatMessage {
                    role: "assistant".to_string(),
                    content: llama_response.content.trim().to_string(),
                },
                finish_reason: if llama_response.stop {
                    "stop".to_string()
                } else {
                    "length".to_string()
                },
            }],
        };

        Ok(HttpResponse::Ok().json(openai_response))
    } else {
        let error_text = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());

        error!("Llama.cpp server error ({}): {}", status, error_text);

        let actix_status = actix_web::http::StatusCode::from_u16(status.as_u16())
            .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);

        Ok(HttpResponse::build(actix_status).json(serde_json::json!({
            "error": {
                "message": error_text,
                "type": "server_error"
            }
        })))
    }
}

// OpenAI Embedding Request - Modified to handle both string and array inputs
#[derive(Debug, Deserialize)]
pub struct EmbeddingRequest {
    #[serde(deserialize_with = "deserialize_input")]
    pub input: Vec<String>,
    pub model: String,
    #[serde(default)]
    pub _encoding_format: Option<String>,
}

// Custom deserializer to handle both string and array inputs
fn deserialize_input<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct InputVisitor;

    impl<'de> Visitor<'de> for InputVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string or an array of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(vec![value.to_string()])
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(vec![value])
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut vec = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                vec.push(value);
            }
            Ok(vec)
        }
    }

    deserializer.deserialize_any(InputVisitor)
}

// OpenAI Embedding Response
#[derive(Debug, Serialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: Usage,
}

#[derive(Debug, Serialize)]
pub struct EmbeddingData {
    pub object: String,
    pub embedding: Vec<f32>,
    pub index: usize,
}

#[derive(Debug, Serialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}

// Llama.cpp Embedding Request
#[derive(Debug, Serialize)]
struct LlamaCppEmbeddingRequest {
    pub content: String,
}

// FIXED: Handle the stupid nested array format
#[derive(Debug, Deserialize)]
struct LlamaCppEmbeddingResponseItem {
    pub index: usize,
    pub embedding: Vec<Vec<f32>>, // This is the  up part - embedding is an array of arrays
}

// Proxy endpoint for embeddings
#[post("/v1/embeddings")]
pub async fn embeddings_local(
    req_body: web::Json<EmbeddingRequest>,
    _req: HttpRequest,
) -> Result<HttpResponse> {
    dotenv().ok();

    // Get llama.cpp server URL
    let llama_url =
        env::var("EMBEDDING_URL").unwrap_or_else(|_| "http://localhost:8082".to_string());

    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|e| {
            error!("Error creating HTTP client: {}", e);
            actix_web::error::ErrorInternalServerError("Failed to create HTTP client")
        })?;

    // Process each input text and get embeddings
    let mut embeddings_data = Vec::new();
    let mut total_tokens = 0;

    for (index, input_text) in req_body.input.iter().enumerate() {
        let llama_request = LlamaCppEmbeddingRequest {
            content: input_text.clone(),
        };

        let response = client
            .post(&format!("{}/embedding", llama_url))
            .header("Content-Type", "application/json")
            .json(&llama_request)
            .send()
            .await
            .map_err(|e| {
                error!("Error calling llama.cpp server for embedding: {}", e);
                actix_web::error::ErrorInternalServerError(
                    "Failed to call llama.cpp server for embedding",
                )
            })?;

        let status = response.status();

        if status.is_success() {
            // First, get the raw response text for debugging
            let raw_response = response.text().await.map_err(|e| {
                error!("Error reading response text: {}", e);
                actix_web::error::ErrorInternalServerError("Failed to read response")
            })?;

            // Parse the response as a vector of items with nested arrays
            let llama_response: Vec<LlamaCppEmbeddingResponseItem> =
                serde_json::from_str(&raw_response).map_err(|e| {
                    error!("Error parsing llama.cpp embedding response: {}", e);
                    error!("Raw response: {}", raw_response);
                    actix_web::error::ErrorInternalServerError(
                        "Failed to parse llama.cpp embedding response",
                    )
                })?;

            // Extract the embedding from the nested array bullshit
            if let Some(item) = llama_response.get(0) {
                // The embedding field contains Vec<Vec<f32>>, so we need to flatten it
                // If it's [[0.1, 0.2, 0.3]], we want [0.1, 0.2, 0.3]
                let flattened_embedding = if !item.embedding.is_empty() {
                    item.embedding[0].clone() // Take the first (and probably only) inner array
                } else {
                    vec![] // Empty if no embedding data
                };

                // Estimate token count
                let estimated_tokens = (input_text.len() as f32 / 4.0).ceil() as u32;
                total_tokens += estimated_tokens;

                embeddings_data.push(EmbeddingData {
                    object: "embedding".to_string(),
                    embedding: flattened_embedding,
                    index,
                });
            } else {
                error!("No embedding data returned for input: {}", input_text);
                return Ok(HttpResponse::InternalServerError().json(serde_json::json!({
                    "error": {
                        "message": format!("No embedding data returned for input {}", index),
                        "type": "server_error"
                    }
                })));
            }
        } else {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

            error!("Llama.cpp server error ({}): {}", status, error_text);

            let actix_status = actix_web::http::StatusCode::from_u16(status.as_u16())
                .unwrap_or(actix_web::http::StatusCode::INTERNAL_SERVER_ERROR);

            return Ok(HttpResponse::build(actix_status).json(serde_json::json!({
                "error": {
                    "message": format!("Failed to get embedding for input {}: {}", index, error_text),
                    "type": "server_error"
                }
            })));
        }
    }

    // Build OpenAI-compatible response
    let openai_response = EmbeddingResponse {
        object: "list".to_string(),
        data: embeddings_data,
        model: req_body.model.clone(),
        usage: Usage {
            prompt_tokens: total_tokens,
            total_tokens,
        },
    };

    Ok(HttpResponse::Ok().json(openai_response))
}

