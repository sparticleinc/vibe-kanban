//! Gemini executor implementation
//!
//! This module provides Gemini CLI-based task execution with streaming support.

mod config;
mod streaming;

use std::{process::Stdio, time::Instant};

use async_trait::async_trait;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use config::{
    max_chunk_size, max_display_size, max_latency_ms, max_message_size, GeminiStreamConfig,
};
// Re-export for external use
use serde_json::Value;
pub use streaming::GeminiPatchBatch;
use streaming::GeminiStreaming;
use tokio::{io::AsyncWriteExt, process::Command};
use uuid::Uuid;

use crate::{
    executor::{
        Executor, ExecutorError, NormalizedConversation, NormalizedEntry, NormalizedEntryType,
    },
    models::task::Task,
    utils::shell::get_shell_command,
};

/// An executor that uses Gemini CLI to process tasks
pub struct GeminiExecutor;

/// An executor that continues a Gemini task with context from previous execution
pub struct GeminiFollowupExecutor {
    pub attempt_id: Uuid,
    pub prompt: String,
}

#[async_trait]
impl Executor for GeminiExecutor {
    async fn spawn(
        &self,
        pool: &sqlx::SqlitePool,
        task_id: Uuid,
        worktree_path: &str,
    ) -> Result<AsyncGroupChild, ExecutorError> {
        // Get the task to fetch its description
        let task = Task::find_by_id(pool, task_id)
            .await?
            .ok_or(ExecutorError::TaskNotFound)?;

        let prompt = if let Some(task_description) = task.description {
            format!(
                r#"project_id: {}
            
Task title: {}
Task description: {}"#,
                task.project_id, task.title, task_description
            )
        } else {
            format!(
                r#"project_id: {}
            
Task title: {}"#,
                task.project_id, task.title
            )
        };

        let mut command = Self::create_gemini_command(worktree_path);

        let mut child = command
            .group_spawn() // Create new process group so we can kill entire tree
            .map_err(|e| {
                crate::executor::SpawnContext::from_command(&command, "Gemini")
                    .with_task(task_id, Some(task.title.clone()))
                    .with_context("Gemini CLI execution for new task")
                    .spawn_error(e)
            })?;

        // Write prompt to stdin
        if let Some(mut stdin) = child.inner().stdin.take() {
            tracing::debug!(
                "Writing prompt to Gemini stdin for task {}: {:?}",
                task_id,
                prompt
            );
            stdin.write_all(prompt.as_bytes()).await.map_err(|e| {
                let context = crate::executor::SpawnContext::from_command(&command, "Gemini")
                    .with_task(task_id, Some(task.title.clone()))
                    .with_context("Failed to write prompt to Gemini CLI stdin");
                ExecutorError::spawn_failed(e, context)
            })?;
            stdin.shutdown().await.map_err(|e| {
                let context = crate::executor::SpawnContext::from_command(&command, "Gemini")
                    .with_task(task_id, Some(task.title.clone()))
                    .with_context("Failed to close Gemini CLI stdin");
                ExecutorError::spawn_failed(e, context)
            })?;
            tracing::info!(
                "Successfully sent prompt to Gemini stdin for task {}",
                task_id
            );
        }

        Ok(child)
    }

    async fn execute_streaming(
        &self,
        pool: &sqlx::SqlitePool,
        task_id: Uuid,
        attempt_id: Uuid,
        execution_process_id: Uuid,
        worktree_path: &str,
    ) -> Result<AsyncGroupChild, ExecutorError> {
        tracing::info!(
            "Starting Gemini execution for task {} attempt {}",
            task_id,
            attempt_id
        );

        Self::update_session_id(pool, execution_process_id, &attempt_id.to_string()).await;

        let mut child = self.spawn(pool, task_id, worktree_path).await?;

        tracing::info!(
            "Gemini process spawned successfully for attempt {}, PID: {:?}",
            attempt_id,
            child.inner().id()
        );

        Self::setup_streaming(pool, &mut child, attempt_id, execution_process_id);

        Ok(child)
    }

    fn normalize_logs(
        &self,
        logs: &str,
        _worktree_path: &str,
    ) -> Result<NormalizedConversation, String> {
        let mut entries: Vec<NormalizedEntry> = Vec::new();
        let mut parse_errors = Vec::new();

        for (line_num, line) in logs.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Try to parse as JSON first (for NormalizedEntry format)
            if trimmed.starts_with('{') {
                match serde_json::from_str::<NormalizedEntry>(trimmed) {
                    Ok(entry) => {
                        entries.push(entry);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse JSONL line {} in Gemini logs: {} - Line: {}",
                            line_num + 1,
                            e,
                            trimmed
                        );
                        parse_errors.push(format!("Line {}: {}", line_num + 1, e));

                        // Create a fallback entry for unrecognized JSON
                        let fallback_entry = NormalizedEntry {
                            timestamp: Some(chrono::Utc::now().to_rfc3339()),
                            entry_type: NormalizedEntryType::SystemMessage,
                            content: format!("Raw output: {}", trimmed),
                            metadata: None,
                        };
                        entries.push(fallback_entry);
                    }
                }
            } else {
                // For non-JSON lines, treat as plain text content
                let text_entry = NormalizedEntry {
                    timestamp: Some(chrono::Utc::now().to_rfc3339()),
                    entry_type: NormalizedEntryType::AssistantMessage,
                    content: trimmed.to_string(),
                    metadata: None,
                };
                entries.push(text_entry);
            }
        }

        if !parse_errors.is_empty() {
            tracing::warn!(
                "Gemini normalize_logs encountered {} parse errors: {}",
                parse_errors.len(),
                parse_errors.join("; ")
            );
        }

        tracing::debug!(
            "Gemini normalize_logs processed {} lines, created {} entries",
            logs.lines().count(),
            entries.len()
        );

        Ok(NormalizedConversation {
            entries,
            session_id: None, // Session ID is managed directly via database, not extracted from logs
            executor_type: "gemini".to_string(),
            prompt: None,
            summary: None,
        })
    }

    // Note: Gemini streaming is handled by the Gemini-specific WAL system.
    // See emit_content_batch() method which calls GeminiExecutor::push_patch().
}

impl GeminiExecutor {
    /// Create a standardized Gemini CLI command
    fn create_gemini_command(worktree_path: &str) -> Command {
        let (shell_cmd, shell_arg) = get_shell_command();
        let gemini_command = "npx @google/gemini-cli@latest --yolo";

        let mut command = Command::new(shell_cmd);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(worktree_path)
            .arg(shell_arg)
            .arg(gemini_command)
            .env("NODE_NO_WARNINGS", "1");
        command
    }

    /// Update executor session ID with error handling
    async fn update_session_id(
        pool: &sqlx::SqlitePool,
        execution_process_id: Uuid,
        session_id: &str,
    ) {
        if let Err(e) = crate::models::executor_session::ExecutorSession::update_session_id(
            pool,
            execution_process_id,
            session_id,
        )
        .await
        {
            tracing::error!(
                "Failed to update session ID for Gemini execution process {}: {}",
                execution_process_id,
                e
            );
        } else {
            tracing::info!(
                "Updated session ID {} for Gemini execution process {}",
                session_id,
                execution_process_id
            );
        }
    }

    /// Setup streaming for both stdout and stderr
    fn setup_streaming(
        pool: &sqlx::SqlitePool,
        child: &mut AsyncGroupChild,
        attempt_id: Uuid,
        execution_process_id: Uuid,
    ) {
        // Take stdout and stderr pipes for streaming
        let stdout = child
            .inner()
            .stdout
            .take()
            .expect("Failed to take stdout from child process");
        let stderr = child
            .inner()
            .stderr
            .take()
            .expect("Failed to take stderr from child process");

        // Start streaming tasks with Gemini-specific line-based message updates
        let pool_clone1 = pool.clone();
        let pool_clone2 = pool.clone();

        tokio::spawn(Self::stream_gemini_chunked(
            stdout,
            pool_clone1,
            attempt_id,
            execution_process_id,
        ));
        // Use default stderr streaming (no custom parsing)
        tokio::spawn(crate::executor::stream_output_to_db(
            stderr,
            pool_clone2,
            attempt_id,
            execution_process_id,
            false,
        ));
    }

    /// Push patches to the Gemini WAL system
    pub fn push_patch(execution_process_id: Uuid, patches: Vec<Value>, content_length: usize) {
        GeminiStreaming::push_patch(execution_process_id, patches, content_length);
    }

    /// Get WAL batches for an execution process, optionally filtering by cursor
    pub fn get_wal_batches(
        execution_process_id: Uuid,
        after_batch_id: Option<u64>,
    ) -> Option<Vec<GeminiPatchBatch>> {
        GeminiStreaming::get_wal_batches(execution_process_id, after_batch_id)
    }

    /// Clean up WAL when execution process finishes
    pub async fn finalize_execution(
        pool: &sqlx::SqlitePool,
        execution_process_id: Uuid,
        final_buffer: &str,
    ) {
        GeminiStreaming::finalize_execution(pool, execution_process_id, final_buffer).await;
    }

    /// Find the best boundary to split a chunk (newline preferred, sentence fallback)
    pub fn find_chunk_boundary(buffer: &str, max_size: usize) -> usize {
        GeminiStreaming::find_chunk_boundary(buffer, max_size)
    }

    /// Conditionally flush accumulated content to database in chunks
    pub async fn maybe_flush_chunk(
        pool: &sqlx::SqlitePool,
        execution_process_id: Uuid,
        buffer: &mut String,
        config: &GeminiStreamConfig,
    ) {
        GeminiStreaming::maybe_flush_chunk(pool, execution_process_id, buffer, config).await;
    }

    /// Emit JSON patch for current message state - either "replace" for growing message or "add" for new message.
    fn emit_message_patch(
        execution_process_id: Uuid,
        current_message: &str,
        entry_count: &mut usize,
        force_new_message: bool,
    ) {
        if current_message.is_empty() {
            return;
        }

        if force_new_message && *entry_count > 0 {
            // Start new message: add new entry to array
            *entry_count += 1;
            let patch_vec = vec![serde_json::json!({
                "op": "add",
                "path": format!("/entries/{}", *entry_count - 1),
                "value": {
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "entry_type": {"type": "assistant_message"},
                    "content": current_message,
                    "metadata": null,
                }
            })];

            Self::push_patch(execution_process_id, patch_vec, current_message.len());
        } else {
            // Growing message: replace current entry
            if *entry_count == 0 {
                *entry_count = 1; // Initialize first message
            }

            let patch_vec = vec![serde_json::json!({
                "op": "replace",
                "path": format!("/entries/{}", *entry_count - 1),
                "value": {
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "entry_type": {"type": "assistant_message"},
                    "content": current_message,
                    "metadata": null,
                }
            })];

            Self::push_patch(execution_process_id, patch_vec, current_message.len());
        }
    }

    /// Emit final content when stream ends
    async fn emit_final_content(
        execution_process_id: Uuid,
        remaining_content: &str,
        entry_count: &mut usize,
    ) {
        if !remaining_content.trim().is_empty() {
            Self::emit_message_patch(
                execution_process_id,
                remaining_content,
                entry_count,
                false, // Don't force new message for final content
            );
        }
    }
}

impl GeminiFollowupExecutor {
    async fn load_task(
        &self,
        pool: &sqlx::SqlitePool,
        task_id: Uuid,
    ) -> Result<Task, ExecutorError> {
        Task::find_by_id(pool, task_id)
            .await?
            .ok_or(ExecutorError::TaskNotFound)
    }

    async fn collect_resume_context(
        &self,
        pool: &sqlx::SqlitePool,
        task: &Task,
    ) -> Result<crate::models::task_attempt::AttemptResumeContext, ExecutorError> {
        crate::models::task_attempt::TaskAttempt::get_attempt_resume_context(
            pool,
            self.attempt_id,
            task.id,
            task.project_id,
        )
        .await
        .map_err(ExecutorError::from)
    }

    fn build_comprehensive_prompt(
        &self,
        task: &Task,
        resume_context: &crate::models::task_attempt::AttemptResumeContext,
    ) -> String {
        format!(
            r#"RESUME CONTEXT FOR CONTINUING TASK

=== TASK INFORMATION ===
Project ID: {}
Task ID: {}
Task Title: {}
Task Description: {}

=== EXECUTION HISTORY ===
The following is the execution history from this task attempt:

{}

=== CURRENT CHANGES ===
The following git diff shows changes made from the base branch to the current state:

```diff
{}
```

=== CURRENT REQUEST ===
{}

=== INSTRUCTIONS ===
You are continuing work on the above task. The execution history shows what has been done previously, and the git diff shows the current state of all changes. Please continue from where the previous execution left off, taking into account all the context provided above.
"#,
            task.project_id,
            task.id,
            task.title,
            task.description
                .as_deref()
                .unwrap_or("No description provided"),
            if resume_context.execution_history.trim().is_empty() {
                "(No previous execution history)"
            } else {
                &resume_context.execution_history
            },
            if resume_context.cumulative_diffs.trim().is_empty() {
                "(No changes detected)"
            } else {
                &resume_context.cumulative_diffs
            },
            self.prompt
        )
    }

    async fn spawn_process(
        &self,
        worktree_path: &str,
        comprehensive_prompt: &str,
    ) -> Result<AsyncGroupChild, ExecutorError> {
        tracing::info!(
            "Spawning Gemini followup execution for attempt {} with resume context ({} chars)",
            self.attempt_id,
            comprehensive_prompt.len()
        );

        let mut command = GeminiExecutor::create_gemini_command(worktree_path);

        let mut child = command.group_spawn().map_err(|e| {
            crate::executor::SpawnContext::from_command(&command, "Gemini")
                .with_context(format!(
                    "Gemini CLI followup execution with context for attempt {}",
                    self.attempt_id
                ))
                .spawn_error(e)
        })?;

        self.send_prompt_to_stdin(&mut child, &command, comprehensive_prompt)
            .await?;
        Ok(child)
    }

    async fn send_prompt_to_stdin(
        &self,
        child: &mut AsyncGroupChild,
        command: &Command,
        comprehensive_prompt: &str,
    ) -> Result<(), ExecutorError> {
        if let Some(mut stdin) = child.inner().stdin.take() {
            tracing::debug!(
                "Sending resume context to Gemini for attempt {}: {} characters",
                self.attempt_id,
                comprehensive_prompt.len()
            );

            stdin
                .write_all(comprehensive_prompt.as_bytes())
                .await
                .map_err(|e| {
                    let context = crate::executor::SpawnContext::from_command(command, "Gemini")
                        .with_context(format!(
                            "Failed to write resume prompt to Gemini CLI stdin for attempt {}",
                            self.attempt_id
                        ));
                    ExecutorError::spawn_failed(e, context)
                })?;

            stdin.shutdown().await.map_err(|e| {
                let context = crate::executor::SpawnContext::from_command(command, "Gemini")
                    .with_context(format!(
                        "Failed to close Gemini CLI stdin for attempt {}",
                        self.attempt_id
                    ));
                ExecutorError::spawn_failed(e, context)
            })?;

            tracing::info!(
                "Successfully sent resume context to Gemini for attempt {}",
                self.attempt_id
            );
        }

        Ok(())
    }
}

#[async_trait]
impl Executor for GeminiFollowupExecutor {
    async fn spawn(
        &self,
        pool: &sqlx::SqlitePool,
        task_id: Uuid,
        worktree_path: &str,
    ) -> Result<AsyncGroupChild, ExecutorError> {
        let task = self.load_task(pool, task_id).await?;
        let resume_context = self.collect_resume_context(pool, &task).await?;
        let comprehensive_prompt = self.build_comprehensive_prompt(&task, &resume_context);
        self.spawn_process(worktree_path, &comprehensive_prompt)
            .await
    }

    async fn execute_streaming(
        &self,
        pool: &sqlx::SqlitePool,
        task_id: Uuid,
        attempt_id: Uuid,
        execution_process_id: Uuid,
        worktree_path: &str,
    ) -> Result<AsyncGroupChild, ExecutorError> {
        tracing::info!(
            "Starting Gemini followup execution for task {} attempt {} with resume context",
            task_id,
            attempt_id
        );

        // Update ExecutorSession with the session_id immediately
        GeminiExecutor::update_session_id(pool, execution_process_id, &self.attempt_id.to_string())
            .await;

        let mut child = self.spawn(pool, task_id, worktree_path).await?;

        tracing::info!(
            "Gemini followup process spawned successfully for attempt {}, PID: {:?}",
            attempt_id,
            child.inner().id()
        );

        GeminiExecutor::setup_streaming(pool, &mut child, attempt_id, execution_process_id);

        Ok(child)
    }

    fn normalize_logs(
        &self,
        logs: &str,
        worktree_path: &str,
    ) -> Result<NormalizedConversation, String> {
        // Reuse the same logic as the main GeminiExecutor
        let main_executor = GeminiExecutor;
        main_executor.normalize_logs(logs, worktree_path)
    }
}

impl GeminiExecutor {
    /// Format Gemini CLI output by inserting line breaks where periods are directly
    /// followed by capital letters (common Gemini CLI formatting issue).
    /// Handles both intra-chunk and cross-chunk period-to-capital transitions.
    fn format_gemini_output(content: &str, accumulated_message: &str) -> String {
        let mut result = String::with_capacity(content.len() + 100); // Reserve some extra space for potential newlines
        let chars: Vec<char> = content.chars().collect();

        // Check for cross-chunk boundary: previous chunk ended with period, current starts with capital
        if !accumulated_message.is_empty() && !content.is_empty() {
            let ends_with_period = accumulated_message.ends_with('.');
            let starts_with_capital = chars
                .first()
                .map(|&c| c.is_uppercase() && c.is_alphabetic())
                .unwrap_or(false);

            if ends_with_period && starts_with_capital {
                result.push('\n');
            }
        }

        // Handle intra-chunk period-to-capital transitions
        for i in 0..chars.len() {
            result.push(chars[i]);

            // Check if current char is '.' and next char is uppercase letter (no space between)
            if chars[i] == '.' && i + 1 < chars.len() {
                let next_char = chars[i + 1];
                if next_char.is_uppercase() && next_char.is_alphabetic() {
                    result.push('\n');
                }
            }
        }

        result
    }

    /// Stream Gemini output with dual-buffer approach: chunks for UI updates, messages for storage.
    ///
    /// **Chunks** (~2KB): Frequent UI updates using "replace" patches for smooth streaming
    /// **Messages** (~8KB): Logical boundaries using "add" patches for new entries
    /// **Consistent WAL/DB**: Both systems see same message structure via JSON patches
    pub async fn stream_gemini_chunked(
        mut output: impl tokio::io::AsyncRead + Unpin,
        pool: sqlx::SqlitePool,
        attempt_id: Uuid,
        execution_process_id: Uuid,
    ) {
        use tokio::io::{AsyncReadExt, BufReader};

        let chunk_limit = max_chunk_size();
        let display_chunk_size = max_display_size(); // ~2KB for UI updates
        let message_boundary_size = max_message_size(); // ~8KB for new message boundaries
        let max_latency = std::time::Duration::from_millis(max_latency_ms());

        let mut reader = BufReader::new(&mut output);

        // Dual buffers: chunk buffer for UI, message buffer for DB
        let mut current_message = String::new(); // Current assistant message content
        let mut db_buffer = String::new(); // Buffer for database storage (using ChunkStore)
        let mut entry_count = 0usize; // Track assistant message entries

        let mut read_buf = vec![0u8; chunk_limit.min(max_chunk_size())]; // Use configurable chunk limit, capped for memory efficiency
        let mut last_chunk_emit = Instant::now();

        // Configuration for WAL and DB management
        let config = GeminiStreamConfig::default();

        tracing::info!(
            "Starting dual-buffer Gemini streaming for attempt {} (chunks: {}B, messages: {}B)",
            attempt_id,
            display_chunk_size,
            message_boundary_size
        );

        loop {
            match reader.read(&mut read_buf).await {
                Ok(0) => {
                    // EOF: emit final content and flush to database
                    Self::emit_final_content(
                        execution_process_id,
                        &current_message,
                        &mut entry_count,
                    )
                    .await;

                    // Flush any remaining database buffer
                    Self::finalize_execution(&pool, execution_process_id, &db_buffer).await;
                    break;
                }
                Ok(n) => {
                    // Convert bytes to string and apply Gemini-specific formatting
                    let raw_chunk = String::from_utf8_lossy(&read_buf[..n]);
                    let formatted_chunk = Self::format_gemini_output(&raw_chunk, &current_message);

                    // Add to both buffers
                    current_message.push_str(&formatted_chunk);
                    db_buffer.push_str(&formatted_chunk);

                    // 1. Check for chunk emission (frequent UI updates ~2KB)
                    let should_emit_chunk = current_message.len() >= display_chunk_size
                        || (last_chunk_emit.elapsed() >= max_latency
                            && !current_message.is_empty());

                    if should_emit_chunk {
                        // Emit "replace" patch for growing message (smooth UI)
                        Self::emit_message_patch(
                            execution_process_id,
                            &current_message,
                            &mut entry_count,
                            false, // Not forcing new message
                        );
                        last_chunk_emit = Instant::now();
                    }

                    // 2. Check for message boundary (new assistant message ~8KB)
                    let should_start_new_message = current_message.len() >= message_boundary_size;

                    if should_start_new_message {
                        // Find optimal boundary for new message
                        let boundary =
                            Self::find_chunk_boundary(&current_message, message_boundary_size);

                        if boundary > 0 && boundary < current_message.len() {
                            // Split at boundary: complete current message, start new one
                            let completed_message = current_message[..boundary].to_string();
                            let remaining_content = current_message[boundary..].to_string();

                            // CRITICAL FIX: Only emit "replace" patch to complete current message
                            // Do NOT emit "add" patch as it shifts existing database entries
                            Self::emit_message_patch(
                                execution_process_id,
                                &completed_message,
                                &mut entry_count,
                                false, // Complete current message
                            );

                            // Store the completed message to database
                            // This ensures the database gets the completed content at the boundary
                            Self::maybe_flush_chunk(
                                &pool,
                                execution_process_id,
                                &mut db_buffer,
                                &config,
                            )
                            .await;

                            // Start fresh message with remaining content (no WAL patch yet)
                            // Next chunk emission will create "replace" patch for entry_count + 1
                            current_message = remaining_content;
                            entry_count += 1; // Move to next entry index for future patches
                        }
                    }

                    // 3. Flush to database (same boundary detection)
                    Self::maybe_flush_chunk(&pool, execution_process_id, &mut db_buffer, &config)
                        .await;
                }
                Err(e) => {
                    tracing::error!(
                        "Error reading stdout for Gemini attempt {}: {}",
                        attempt_id,
                        e
                    );
                    break;
                }
            }
        }

        tracing::info!(
            "Dual-buffer Gemini streaming completed for attempt {} ({} messages)",
            attempt_id,
            entry_count
        );
    }
}
