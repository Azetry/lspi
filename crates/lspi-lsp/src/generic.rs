use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use lspi_core::hashing::sha256_hex;
use serde_json::Value;
use tokio::fs;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tracing::debug;

use crate::lsp::{
    LspClient, LspClientOptions, LspDiagnostic, LspPosition, LspTextEdit, normalize_workspace_edit,
};
use crate::symbol::{
    CallHierarchyIncomingCallMatch, CallHierarchyIncomingResult, CallHierarchyItemResolved,
    CallHierarchyOutgoingCallMatch, CallHierarchyOutgoingResult, DefinitionMatch, FlatSymbol,
    ReferenceMatch, RenameCandidate, ResolvedLocation, ResolvedSymbol, WorkspaceSymbolMatch,
    parse_call_hierarchy_item, parse_incoming_calls, parse_locations, parse_outgoing_calls,
    parse_symbols, parse_workspace_symbols, to_lsp_location, to_resolved_location,
};

#[derive(Debug, Clone)]
pub struct GenericLspClientOptions {
    pub command: String,
    pub args: Vec<String>,
    pub root_dir: PathBuf,
    pub process_cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub workspace_folders: Vec<PathBuf>,
    pub adapter: crate::adapter::LspAdapter,
    pub initialize_timeout: Duration,
    pub request_timeout: Duration,
    pub request_timeout_overrides: HashMap<String, Duration>,
    pub language_id: String,
    pub warmup_delay: Duration,
    pub workspace_configuration: HashMap<String, Value>,
    pub initialize_options: Option<Value>,
    pub client_capabilities: Option<Value>,
}

pub struct GenericLspClient {
    lsp: LspClient,
    open_files: Mutex<HashMap<PathBuf, OpenFileState>>,
    language_id: String,
    warmup_delay: Duration,
}

#[derive(Debug, Clone)]
struct OpenFileState {
    version: i32,
    last_sha256: String,
}

impl GenericLspClient {
    pub async fn start(options: GenericLspClientOptions) -> Result<Self> {
        if options.command.trim().is_empty() {
            return Err(anyhow!("generic LSP command must not be empty"));
        }
        if options.language_id.trim().is_empty() {
            return Err(anyhow!("generic LSP language_id must not be empty"));
        }

        let lsp = LspClient::start(LspClientOptions {
            command: options.command,
            args: options.args,
            process_cwd: options.process_cwd,
            root_dir: options.root_dir,
            env: options.env,
            workspace_folders: options.workspace_folders,
            adapter: options.adapter,
            initialize_timeout: options.initialize_timeout,
            request_timeout: options.request_timeout,
            request_timeout_overrides: options.request_timeout_overrides,
            workspace_configuration: options.workspace_configuration,
            initialize_options: options.initialize_options,
            client_capabilities: options.client_capabilities,
        })
        .await?;

        Ok(Self {
            lsp,
            open_files: Mutex::new(HashMap::new()),
            language_id: options.language_id,
            warmup_delay: options.warmup_delay,
        })
    }

    pub async fn shutdown(self) -> Result<()> {
        self.lsp.shutdown().await
    }

    pub async fn find_definition_by_name(
        &self,
        file_path: &Path,
        symbol_name: &str,
        symbol_kind: Option<u32>,
        max_symbols: usize,
    ) -> Result<Vec<DefinitionMatch>> {
        self.prepare_file(file_path).await?;

        let symbols = self.document_symbols_with_retry(file_path).await?;

        let mut matches = Vec::new();
        for sym in symbols
            .into_iter()
            .filter(|s| s.name == symbol_name)
            .filter(|s| symbol_kind.map(|k| k == s.kind).unwrap_or(true))
            .take(max_symbols)
        {
            let pos = sym.selection_range.start.clone();
            let defs = self.definition_values_with_retry(file_path, pos).await?;
            let mut resolved = Vec::new();
            for def in defs {
                let lsp_loc = to_lsp_location(&def)?;
                if let Ok(r) = to_resolved_location(&lsp_loc) {
                    resolved.push(r);
                }
            }

            matches.push(DefinitionMatch {
                symbol: ResolvedSymbol {
                    name: sym.name,
                    kind: sym.kind,
                    range: sym.range,
                    selection_range: sym.selection_range,
                },
                definitions: resolved,
            });
        }

        Ok(matches)
    }

    pub async fn find_references_by_name(
        &self,
        file_path: &Path,
        symbol_name: &str,
        symbol_kind: Option<u32>,
        include_declaration: bool,
        max_symbols: usize,
        max_references: usize,
    ) -> Result<Vec<ReferenceMatch>> {
        self.prepare_file(file_path).await?;

        let symbols = self.document_symbols_with_retry(file_path).await?;

        let mut results = Vec::new();
        let mut remaining = max_references.max(1);

        for sym in symbols
            .into_iter()
            .filter(|s| s.name == symbol_name)
            .filter(|s| symbol_kind.map(|k| k == s.kind).unwrap_or(true))
            .take(max_symbols)
        {
            if remaining == 0 {
                break;
            }

            let pos = sym.selection_range.start.clone();
            let refs = self
                .reference_values_with_retry(file_path, pos, include_declaration)
                .await?;
            let mut references = Vec::new();
            let mut truncated = false;

            for r in refs {
                let lsp_loc = to_lsp_location(&r)?;
                if let Ok(resolved) = to_resolved_location(&lsp_loc) {
                    references.push(resolved);
                    remaining = remaining.saturating_sub(1);
                    if remaining == 0 {
                        truncated = true;
                        break;
                    }
                }
            }

            results.push(ReferenceMatch {
                symbol: ResolvedSymbol {
                    name: sym.name,
                    kind: sym.kind,
                    range: sym.range,
                    selection_range: sym.selection_range,
                },
                references,
                truncated,
            });
        }

        Ok(results)
    }

    pub async fn definition_at(
        &self,
        file_path: &Path,
        position: LspPosition,
        max_definitions: usize,
    ) -> Result<Vec<ResolvedLocation>> {
        self.prepare_file(file_path).await?;

        let defs = self
            .definition_values_with_retry(file_path, position)
            .await?;

        let mut resolved = Vec::new();
        for def in defs.into_iter().take(max_definitions.max(1)) {
            let lsp_loc = to_lsp_location(&def)?;
            if let Ok(r) = to_resolved_location(&lsp_loc) {
                resolved.push(r);
            }
        }
        Ok(resolved)
    }

    pub async fn references_at(
        &self,
        file_path: &Path,
        position: LspPosition,
        include_declaration: bool,
        max_references: usize,
    ) -> Result<(Vec<ResolvedLocation>, bool)> {
        self.prepare_file(file_path).await?;

        let refs = self
            .reference_values_with_retry(file_path, position, include_declaration)
            .await?;

        let mut resolved = Vec::new();
        let mut truncated = false;
        let limit = max_references.max(1);
        for def in refs {
            if resolved.len() >= limit {
                truncated = true;
                break;
            }
            let lsp_loc = to_lsp_location(&def)?;
            if let Ok(r) = to_resolved_location(&lsp_loc) {
                resolved.push(r);
            }
        }
        Ok((resolved, truncated))
    }

    pub async fn implementation_at(
        &self,
        file_path: &Path,
        position: LspPosition,
        max_results: usize,
    ) -> Result<Vec<ResolvedLocation>> {
        self.prepare_file(file_path).await?;

        let values = self
            .implementation_values_with_retry(file_path, position)
            .await?;

        let mut resolved = Vec::new();
        for v in values.into_iter().take(max_results.max(1)) {
            let lsp_loc = to_lsp_location(&v)?;
            if let Ok(r) = to_resolved_location(&lsp_loc) {
                resolved.push(r);
            }
        }
        Ok(resolved)
    }

    pub async fn type_definition_at(
        &self,
        file_path: &Path,
        position: LspPosition,
        max_results: usize,
    ) -> Result<Vec<ResolvedLocation>> {
        self.prepare_file(file_path).await?;

        let values = self
            .type_definition_values_with_retry(file_path, position)
            .await?;

        let mut resolved = Vec::new();
        for v in values.into_iter().take(max_results.max(1)) {
            let lsp_loc = to_lsp_location(&v)?;
            if let Ok(r) = to_resolved_location(&lsp_loc) {
                resolved.push(r);
            }
        }
        Ok(resolved)
    }

    pub async fn hover_at(&self, file_path: &Path, position: LspPosition) -> Result<Value> {
        self.prepare_file(file_path).await?;
        self.lsp.hover(file_path, position).await
    }

    pub async fn incoming_calls_at(
        &self,
        file_path: &Path,
        position: LspPosition,
        max_results: usize,
    ) -> Result<CallHierarchyIncomingResult> {
        self.prepare_file(file_path).await?;
        let prepared = self
            .prepare_call_hierarchy_with_retry(file_path, position)
            .await?;

        let Some(items) = prepared.as_array() else {
            return Ok(CallHierarchyIncomingResult {
                target: None,
                calls: Vec::new(),
            });
        };
        if items.is_empty() {
            return Ok(CallHierarchyIncomingResult {
                target: None,
                calls: Vec::new(),
            });
        }

        let mut last_err: Option<anyhow::Error> = None;
        let mut target: Option<CallHierarchyItemResolved> = None;
        let mut calls: Option<Vec<CallHierarchyIncomingCallMatch>> = None;

        for item in items {
            target = parse_call_hierarchy_item(item).ok();
            match self.lsp.call_hierarchy_incoming_calls(item).await {
                Ok(raw) => {
                    let mut parsed = parse_incoming_calls(raw)?;
                    parsed.truncate(max_results.max(1));
                    calls = Some(parsed);
                    if calls.as_ref().is_some_and(|c| !c.is_empty()) {
                        break;
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }

        let Some(calls) = calls else {
            return Err(last_err.unwrap_or_else(|| anyhow!("incomingCalls failed")));
        };

        Ok(CallHierarchyIncomingResult { target, calls })
    }

    pub async fn outgoing_calls_at(
        &self,
        file_path: &Path,
        position: LspPosition,
        max_results: usize,
    ) -> Result<CallHierarchyOutgoingResult> {
        self.prepare_file(file_path).await?;
        let prepared = self
            .prepare_call_hierarchy_with_retry(file_path, position)
            .await?;

        let Some(items) = prepared.as_array() else {
            return Ok(CallHierarchyOutgoingResult {
                target: None,
                calls: Vec::new(),
            });
        };
        if items.is_empty() {
            return Ok(CallHierarchyOutgoingResult {
                target: None,
                calls: Vec::new(),
            });
        }

        let mut last_err: Option<anyhow::Error> = None;
        let mut target: Option<CallHierarchyItemResolved> = None;
        let mut calls: Option<Vec<CallHierarchyOutgoingCallMatch>> = None;

        for item in items {
            target = parse_call_hierarchy_item(item).ok();
            match self.lsp.call_hierarchy_outgoing_calls(item).await {
                Ok(raw) => {
                    let mut parsed = parse_outgoing_calls(raw)?;
                    parsed.truncate(max_results.max(1));
                    calls = Some(parsed);
                    if calls.as_ref().is_some_and(|c| !c.is_empty()) {
                        break;
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }

        let Some(calls) = calls else {
            return Err(last_err.unwrap_or_else(|| anyhow!("outgoingCalls failed")));
        };

        Ok(CallHierarchyOutgoingResult { target, calls })
    }

    pub async fn document_symbols(
        &self,
        file_path: &Path,
        max_symbols: usize,
    ) -> Result<Vec<ResolvedSymbol>> {
        self.prepare_file(file_path).await?;
        let symbols = self.document_symbols_with_retry(file_path).await?;
        Ok(symbols
            .into_iter()
            .take(max_symbols.max(1))
            .map(|s| ResolvedSymbol {
                name: s.name,
                kind: s.kind,
                range: s.range,
                selection_range: s.selection_range,
            })
            .collect())
    }

    pub async fn workspace_symbols(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WorkspaceSymbolMatch>> {
        let raw = self.lsp.workspace_symbols(query).await?;
        let mut out = parse_workspace_symbols(raw)?;
        out.truncate(max_results.max(1));
        Ok(out)
    }

    pub async fn workspace_symbols_for_file(
        &self,
        file_path: &Path,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<WorkspaceSymbolMatch>> {
        let cold = self.prepare_file(file_path).await?;
        let raw = self
            .lsp
            .workspace_symbols_with_cold_retry(query, cold)
            .await?;
        let mut out = parse_workspace_symbols(raw)?;
        out.truncate(max_results.max(1));
        Ok(out)
    }

    pub async fn get_diagnostics(
        &self,
        file_path: &Path,
        max_wait: Duration,
    ) -> Result<Vec<LspDiagnostic>> {
        self.prepare_file(file_path).await?;

        if let Some(diags) = self.lsp.document_diagnostics(file_path, max_wait).await? {
            return Ok(diags);
        }

        self.lsp
            .wait_for_diagnostics_update(file_path, max_wait)
            .await
    }

    pub async fn list_symbol_candidates(
        &self,
        file_path: &Path,
        symbol_name: &str,
        symbol_kind: Option<u32>,
        max_symbols: usize,
    ) -> Result<Vec<RenameCandidate>> {
        self.prepare_file(file_path).await?;
        let symbols = self.document_symbols_with_retry(file_path).await?;

        Ok(symbols
            .into_iter()
            .filter(|s| s.name == symbol_name)
            .filter(|s| symbol_kind.map(|k| k == s.kind).unwrap_or(true))
            .take(max_symbols)
            .map(|s| RenameCandidate {
                name: s.name,
                kind: s.kind,
                line: s.selection_range.start.line + 1,
                character: s.selection_range.start.character + 1,
                selection_range: s.selection_range,
            })
            .collect())
    }

    pub async fn rename_at(
        &self,
        file_path: &Path,
        position: LspPosition,
        new_name: &str,
    ) -> Result<HashMap<String, Vec<LspTextEdit>>> {
        self.prepare_file(file_path).await?;
        self.rename_at_prepared(file_path, position, new_name).await
    }

    pub async fn rename_at_prepared(
        &self,
        file_path: &Path,
        position: LspPosition,
        new_name: &str,
    ) -> Result<HashMap<String, Vec<LspTextEdit>>> {
        let raw = self.lsp.rename(file_path, position, new_name).await?;
        normalize_workspace_edit(raw)
    }

    async fn prepare_file(&self, file_path: &Path) -> Result<bool> {
        self.open_or_sync(file_path).await
    }

    async fn open_or_sync(&self, file_path: &Path) -> Result<bool> {
        let abs = file_path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize file path: {file_path:?}"))?;
        let content = fs::read(&abs)
            .await
            .with_context(|| format!("failed to read file: {abs:?}"))?;
        let hash = sha256_hex(&content);
        let text = String::from_utf8(content).context("file is not valid UTF-8")?;

        let mut open = self.open_files.lock().await;
        let cold = match open.get_mut(&abs) {
            None => {
                debug!("didOpen {:?}", abs);
                self.lsp.did_open(&abs, &self.language_id, 1, text).await?;
                open.insert(
                    abs,
                    OpenFileState {
                        version: 1,
                        last_sha256: hash,
                    },
                );
                if !self.warmup_delay.is_zero() {
                    tokio::time::sleep(self.warmup_delay).await;
                }
                true
            }
            Some(state) => {
                if state.last_sha256 != hash {
                    state.version += 1;
                    state.last_sha256 = hash;
                    debug!("didChange {:?} version={}", abs, state.version);
                    self.lsp.did_change(&abs, state.version, text).await?;
                }
                false
            }
        };
        Ok(cold)
    }

    async fn document_symbols_with_retry(&self, file_path: &Path) -> Result<Vec<FlatSymbol>> {
        let mut last_err: Option<anyhow::Error> = None;
        let mut delay_ms = 200u64;
        for attempt in 0..10 {
            match self.lsp.document_symbols(file_path).await {
                Ok(value) => match parse_symbols(value) {
                    Ok(symbols) => {
                        if !symbols.is_empty() {
                            return Ok(symbols);
                        }
                        if attempt == 0 && !self.warmup_delay.is_zero() {
                            tokio::time::sleep(self.warmup_delay).await;
                        }
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        delay_ms = (delay_ms + 200).min(1_000);
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(Vec::new())
    }

    async fn definition_values_with_retry(
        &self,
        file_path: &Path,
        position: LspPosition,
    ) -> Result<Vec<Value>> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            match self.lsp.definition(file_path, position.clone()).await {
                Ok(value) => match parse_locations(value) {
                    Ok(defs) => {
                        if !defs.is_empty() {
                            return Ok(defs);
                        }
                        tokio::time::sleep(Duration::from_millis(200 * (attempt + 1) as u64)).await;
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(Vec::new())
    }

    async fn reference_values_with_retry(
        &self,
        file_path: &Path,
        position: LspPosition,
        include_declaration: bool,
    ) -> Result<Vec<Value>> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            match self
                .lsp
                .references(file_path, position.clone(), include_declaration)
                .await
            {
                Ok(value) => match parse_locations(value) {
                    Ok(refs) => {
                        if !refs.is_empty() {
                            return Ok(refs);
                        }
                        tokio::time::sleep(Duration::from_millis(200 * (attempt + 1) as u64)).await;
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(Vec::new())
    }

    async fn implementation_values_with_retry(
        &self,
        file_path: &Path,
        position: LspPosition,
    ) -> Result<Vec<Value>> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            match self.lsp.implementation(file_path, position.clone()).await {
                Ok(value) => match parse_locations(value) {
                    Ok(locs) => {
                        if !locs.is_empty() {
                            return Ok(locs);
                        }
                        tokio::time::sleep(Duration::from_millis(200 * (attempt + 1) as u64)).await;
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(Vec::new())
    }

    async fn type_definition_values_with_retry(
        &self,
        file_path: &Path,
        position: LspPosition,
    ) -> Result<Vec<Value>> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            match self.lsp.type_definition(file_path, position.clone()).await {
                Ok(value) => match parse_locations(value) {
                    Ok(locs) => {
                        if !locs.is_empty() {
                            return Ok(locs);
                        }
                        tokio::time::sleep(Duration::from_millis(200 * (attempt + 1) as u64)).await;
                    }
                    Err(e) => last_err = Some(e),
                },
                Err(e) => last_err = Some(e),
            }
        }

        if let Some(err) = last_err {
            return Err(err);
        }
        Ok(Vec::new())
    }

    async fn prepare_call_hierarchy_with_retry(
        &self,
        file_path: &Path,
        position: LspPosition,
    ) -> Result<Value> {
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..3 {
            match self
                .lsp
                .prepare_call_hierarchy(file_path, position.clone())
                .await
            {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(200 * (attempt + 1) as u64)).await;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("prepareCallHierarchy failed")))
    }
}

#[cfg(all(test, unix))]
mod cold_workspace_symbol_tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{GenericLspClient, GenericLspClientOptions};

    #[tokio::test]
    async fn opens_before_workspace_symbols_and_only_retries_the_cold_file() {
        let root = tempdir().unwrap();
        let file_path = root.path().join("sample.ts");
        std::fs::write(&file_path, "export const TargetSymbol = 1;\n").unwrap();
        let trace_path = root.path().join("trace.log");
        let fixture =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_workspace_lsp.py");

        let client = GenericLspClient::start(GenericLspClientOptions {
            command: "python3".to_string(),
            args: vec![fixture.to_string_lossy().to_string()],
            root_dir: root.path().to_path_buf(),
            process_cwd: root.path().to_path_buf(),
            env: HashMap::from([(
                "LSPI_TEST_TRACE".to_string(),
                trace_path.to_string_lossy().to_string(),
            )]),
            workspace_folders: Vec::new(),
            adapter: crate::adapter::LspAdapter::default(),
            initialize_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(2),
            request_timeout_overrides: HashMap::new(),
            language_id: "typescript".to_string(),
            warmup_delay: Duration::ZERO,
            workspace_configuration: HashMap::new(),
            initialize_options: None,
            client_capabilities: None,
        })
        .await
        .unwrap();

        let matches = client
            .workspace_symbols_for_file(&file_path, "TargetSymbol", 10)
            .await
            .unwrap();
        assert_eq!(matches.len(), 1);

        let before_warm_query = std::fs::read_to_string(&trace_path).unwrap();
        let methods: Vec<_> = before_warm_query.lines().collect();
        let did_open = methods
            .iter()
            .position(|method| *method == "textDocument/didOpen")
            .unwrap();
        let first_workspace = methods
            .iter()
            .position(|method| *method == "workspace/symbol")
            .unwrap();
        assert!(did_open < first_workspace);
        assert_eq!(
            methods
                .iter()
                .filter(|method| **method == "workspace/symbol")
                .count(),
            3
        );

        let missing = client
            .workspace_symbols_for_file(&file_path, "MissingSymbol", 10)
            .await
            .unwrap();
        assert!(missing.is_empty());
        let after_warm_query = std::fs::read_to_string(&trace_path).unwrap();
        assert_eq!(
            after_warm_query
                .lines()
                .filter(|method| *method == "workspace/symbol")
                .count(),
            4
        );

        let second_file = root.path().join("second.ts");
        std::fs::write(&second_file, "export const OtherSymbol = 1;\n").unwrap();
        let error = client
            .workspace_symbols_for_file(&second_file, "AlwaysError", 10)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("still indexing"));
        let after_cold_error = std::fs::read_to_string(&trace_path).unwrap();
        assert_eq!(
            after_cold_error
                .lines()
                .filter(|method| *method == "workspace/symbol")
                .count(),
            13
        );

        client.shutdown().await.unwrap();
    }
}
