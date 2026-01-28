// Copyright 2023 Salesforce, Inc. All rights reserved.
mod generated;

use anyhow::{anyhow, Result};

use pdk::hl::*;
use pdk::logger;

use crate::generated::config::Config;

// Default chunk size: 100KB (102400 bytes)
const DEFAULT_CHUNK_SIZE: usize = 100 * 1024;

/// Response filter that adds chunked encoding to upstream responses.
/// - Reads response from upstream
/// - Adds chunked encoding with configurable chunk size (default 100KB)
/// - Preserves all original response headers from upstream
/// - Fault-tolerant: errors are logged but don't stop message processing
async fn response_filter(response_state: ResponseState, config: &Config) {
    // Check if chunked encoding is enabled via config (defaults to true)
    if !config.enabled.unwrap_or(true) {
        logger::debug!("Chunked encoding policy: Disabled via configuration, passing through unchanged");
        let _headers_state = response_state.into_headers_state().await;
        return;
    }

    // Calculate chunk size from config (convert KB to bytes), default to 100KB
    let chunk_size_kb = config.chunk_size_kb.unwrap_or(100) as usize;
    let chunk_size = if chunk_size_kb == 0 { DEFAULT_CHUNK_SIZE } else { chunk_size_kb * 1024 };

    // Process headers phase - preserve all upstream headers
    let headers_state = response_state.into_headers_state().await;

    // Get the handler to manipulate response headers
    let handler = headers_state.handler();

    // Add Transfer-Encoding: chunked header
    // Note: We add this header to indicate chunked encoding
    handler.set_header("Transfer-Encoding", "chunked");

    // Remove Content-Length header as it's incompatible with chunked encoding
    // This is required per HTTP/1.1 spec - can't have both
    handler.remove_header("Content-Length");

    logger::info!("Chunked encoding policy: Set Transfer-Encoding to chunked (chunk_size={}KB)", chunk_size / 1024);

    // Transition to body state to process the response body
    let body_state = headers_state.into_body_state().await;

    // Read the entire upstream response body
    // This reads the response from upstream before processing
    let original_body = body_state.handler().body();

    let body_len = original_body.len();
    logger::info!("Chunked encoding policy: Read {} bytes from upstream response", body_len);

    // If body is empty, just pass through
    if body_len == 0 {
        logger::info!("Chunked encoding policy: Empty body from upstream, passing through");
        return;
    }

    // Build chunked response body
    // Chunked transfer encoding format:
    // <chunk-size-in-hex>\r\n
    // <chunk-data>\r\n
    // ... repeat for each chunk ...
    // 0\r\n
    // \r\n
    let chunked_body = match build_chunked_body(&original_body, chunk_size) {
        Ok(body) => body,
        Err(e) => {
            // Fault tolerance: on error, pass original body unchanged
            logger::warn!("Chunked encoding policy: Failed to build chunked body: {:?}. Passing original body.", e);
            if let Err(set_err) = body_state.handler().set_body(&original_body) {
                logger::warn!("Chunked encoding policy: Failed to set original body: {:?}", set_err);
            }
            return;
        }
    };

    let chunk_count = (body_len + chunk_size - 1) / chunk_size;
    logger::info!(
        "Chunked encoding policy: Split {} bytes into {} chunks of up to {} bytes ({}KB) each",
        body_len,
        chunk_count,
        chunk_size,
        chunk_size / 1024
    );

    // Set the chunked body
    if let Err(e) = body_state.handler().set_body(&chunked_body) {
        // Fault tolerance: log error but don't stop processing
        logger::warn!("Chunked encoding policy: Failed to set chunked body: {:?}. Message will continue processing.", e);
    }
}

/// Builds a chunked transfer encoded body from the original body bytes.
/// Each chunk is up to `chunk_size` bytes (default 100KB).
fn build_chunked_body(original_body: &[u8], chunk_size: usize) -> Result<Vec<u8>, anyhow::Error> {
    let mut chunked_body = Vec::new();
    let body_len = original_body.len();

    // Process body in chunks of chunk_size
    let mut offset = 0;
    while offset < body_len {
        let chunk_end = std::cmp::min(offset + chunk_size, body_len);
        let chunk = &original_body[offset..chunk_end];
        let chunk_len = chunk.len();

        // Write chunk size in hexadecimal followed by CRLF
        let size_line = format!("{:X}\r\n", chunk_len);
        chunked_body.extend_from_slice(size_line.as_bytes());

        // Write chunk data
        chunked_body.extend_from_slice(chunk);

        // Write CRLF after chunk data
        chunked_body.extend_from_slice(b"\r\n");

        offset = chunk_end;
    }

    // Write final chunk (zero-length) to indicate end of body
    chunked_body.extend_from_slice(b"0\r\n\r\n");

    Ok(chunked_body)
}

/// Request filter - passes through requests unchanged
/// This policy only operates on responses
async fn request_filter(request_state: RequestState, _config: &Config) {
    // Pass through request unchanged - this policy only processes responses
    let _headers_state = request_state.into_headers_state().await;
    logger::debug!("Chunked encoding policy: Request passed through unchanged");
}

#[entrypoint]
async fn configure(launcher: Launcher, Configuration(bytes): Configuration) -> Result<()> {
    let config: Config = serde_json::from_slice(&bytes).map_err(|err| {
        anyhow!(
            "Failed to parse configuration '{}'. Cause: {}",
            String::from_utf8_lossy(&bytes),
            err
        )
    })?;

    // Create a filter that processes both requests (pass-through) and responses (chunked encoding)
    let filter = on_request(|rs| request_filter(rs, &config))
        .on_response(|rs| response_filter(rs, &config));

    launcher.launch(filter).await?;
    Ok(())
}
