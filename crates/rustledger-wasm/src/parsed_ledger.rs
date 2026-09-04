//! Stateful ledger classes for WASM.
//!
//! Provides two classes:
//! - [`ParsedLedger`]: Single-file ledger with full editor features (completions, hover, etc.)
//! - [`Ledger`]: Multi-file ledger for queries and validation (no position-based editor features)

use std::collections::HashMap;
use std::path::Path;
use wasm_bindgen::prelude::*;

use rustledger_core::Directive;
use rustledger_parser::ParseResult as ParserResult;

use crate::cache;
use crate::convert::{directive_to_json, directive_to_json_at};
use crate::editor;
use crate::helpers::{load_and_book, run_validation, to_js};
#[cfg(feature = "plugins")]
use crate::types::PluginResult;
use crate::types::{
    Error, FormatResult, LedgerOptions, PadResult, QueryResult, SourceLocationJson,
};

// =============================================================================
// Shared query/directive logic (used by both ParsedLedger and Ledger)
// =============================================================================

fn execute_query(
    directives: &[Directive],
    query_str: &str,
    account_types: rustledger_core::AccountTypes,
) -> Result<JsValue, JsError> {
    use crate::convert::value_to_cell;
    use rustledger_query::{Executor, parse as parse_query};

    let query = match parse_query(query_str) {
        Ok(q) => q,
        Err(e) => {
            let result = QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                errors: vec![Error::new(e.to_string())],
            };
            return to_js(&result);
        }
    };

    // BQL is a balance-computing consumer; expand pads explicitly
    // before handing the directive list to the executor (#1288).
    // `ParsedLedger.directives` / `Ledger.directives` are
    // source-faithful per the architectural rule documented on
    // `rustledger_loader::Ledger.directives`; the executor needs
    // the expanded view to compute `sum(position)` with pad effects
    // included.
    let expanded = rustledger_booking::merge_with_padding(directives);
    let mut executor = Executor::new(&expanded);
    // Config-aware classification (POSSIGN/ACCOUNT_SORTKEY honor name_*
    // renames) — the wasm wire LedgerOptions deliberately lacks name_*,
    // so callers supply core AccountTypes from their construction source.
    executor.set_account_types(account_types);
    match executor.execute(&query) {
        Ok(result) => {
            let rows: Vec<Vec<_>> = result
                .rows
                .iter()
                .map(|row| row.iter().map(value_to_cell).collect())
                .collect();

            let query_result = QueryResult {
                columns: result.columns,
                rows,
                errors: Vec::new(),
            };
            to_js(&query_result)
        }
        Err(e) => {
            let result = QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                errors: vec![Error::new(format!("Query execution error: {e}"))],
            };
            to_js(&result)
        }
    }
}

fn execute_expand_pads(directives: &[Directive]) -> Result<JsValue, JsError> {
    use rustledger_booking::process_pads;

    let pad_result = process_pads(directives);

    let result = PadResult {
        // The source stream, verbatim — `process_pads` no longer
        // echoes its input back, so read it from the directives we
        // were handed instead of from the result.
        directives: directives.iter().map(directive_to_json).collect(),
        padding_transactions: pad_result
            .padding_transactions
            .iter()
            .map(|txn| directive_to_json(&Directive::Transaction(txn.clone())))
            .collect(),
        errors: pad_result
            .errors
            .iter()
            .map(|e| Error::new(e.message.clone()))
            .collect(),
    };
    to_js(&result)
}

#[cfg(feature = "plugins")]
fn execute_plugin(directives: &[Directive], plugin_name: &str) -> Result<JsValue, JsError> {
    use rustledger_plugin::{
        NativePluginRegistry, PluginInput, PluginOptions, directives_to_wrappers,
        wrappers_to_directives,
    };

    let registry = NativePluginRegistry::global();
    // External API runs plugins on already-booked input — synth
    // plugins are a loader-internal concern and would re-emit Opens
    // for accounts the booking pass already opened.
    let Some(plugin) = registry.find_regular(plugin_name) else {
        let result = PluginResult {
            directives: Vec::new(),
            errors: vec![Error::new(format!("Unknown plugin: {plugin_name}"))],
        };
        return to_js(&result);
    };

    let wrappers = directives_to_wrappers(directives);
    let input = PluginInput {
        directives: wrappers,
        options: PluginOptions::default(),
        config: None,
    };

    let input_dirs = input.directives.clone();
    let output = plugin.process(input);
    let materialized = crate::api::materialize_plugin_ops(&input_dirs, &output);

    let output_directives = match wrappers_to_directives(&materialized) {
        Ok(dirs) => dirs,
        Err(e) => {
            let result = PluginResult {
                directives: Vec::new(),
                errors: vec![Error::new(format!("Conversion error: {e}"))],
            };
            return to_js(&result);
        }
    };

    let result = PluginResult {
        directives: output_directives.iter().map(directive_to_json).collect(),
        errors: output
            .errors
            .iter()
            .map(|e| match e.severity {
                rustledger_plugin::PluginErrorSeverity::Warning => {
                    Error::warning(e.message.clone())
                }
                rustledger_plugin::PluginErrorSeverity::Error => Error::new(e.message.clone()),
            })
            .collect(),
    };
    to_js(&result)
}

/// Where each directive was written, aligned with `directives`.
///
/// The loader hands back `Spanned<Directive>` (a byte range plus the id of
/// the file it was read from) and the source map that resolves those ids to
/// paths and lines; both are dropped when the directives are stored, so the
/// locations are read off here while they are still in hand.
///
/// A directive a plugin synthesized has no source text — the loader marks it
/// with `SYNTHESIZED_FILE_ID` — and gets `None`.
fn locations_of(
    directives: &[rustledger_core::Spanned<Directive>],
    source_map: &rustledger_loader::SourceMap,
) -> Vec<Option<SourceLocationJson>> {
    directives
        .iter()
        .map(|spanned| {
            if spanned.file_id == rustledger_core::SYNTHESIZED_FILE_ID {
                return None;
            }
            let file = source_map.get(spanned.file_id as usize)?;
            let (line, _) = file.line_col(spanned.span.start);
            // The span's end is exclusive and lands after the directive's
            // final newline, which is already the next line. Step back one
            // byte to name the last line the directive actually covers.
            let last = spanned.span.end.saturating_sub(1).max(spanned.span.start);
            let (mut end_line, _) = file.line_col(last);
            // A directive's span runs to the start of the next one, so it
            // swallows the blank lines between them. Those separate the two
            // and belong to neither; drop them. A trailing COMMENT is left
            // in — the parser reads it as part of the directive
            // (`Transaction::trailing_comments`).
            while end_line > line
                && file
                    .line(end_line)
                    .is_some_and(|text| text.trim().is_empty())
            {
                end_line -= 1;
            }

            Some(SourceLocationJson {
                file: file.path.display().to_string(),
                line: u32::try_from(line).unwrap_or(u32::MAX),
                end_line: u32::try_from(end_line).unwrap_or(u32::MAX),
            })
        })
        .collect()
}

// =============================================================================
// ParsedLedger: Single-file with full editor features
// =============================================================================

/// A parsed and validated single-file ledger with editor features.
///
/// Use this class for single-file ledgers where you need completions, hover,
/// go-to-definition, and other editor integration features.
///
/// For multi-file ledgers, use [`Ledger`] instead.
///
/// # Example (JavaScript)
///
/// ```javascript
/// const ledger = new ParsedLedger(source);
/// if (ledger.isValid()) {
///     const balances = ledger.query("BALANCES");
///     const completions = ledger.getCompletions(line, char);
/// }
/// ```
#[wasm_bindgen(skip_typescript)]
pub struct ParsedLedger {
    /// The original source text.
    source: String,
    /// The raw parse result (for editor features).
    parse_result: ParserResult,
    /// The booked directives.
    directives: Vec<Directive>,
    /// Ledger options.
    options: LedgerOptions,
    /// Parse errors.
    parse_errors: Vec<Error>,
    /// Validation errors.
    validation_errors: Vec<Error>,
    /// Cached editor data (accounts, currencies, payees, line index).
    editor_cache: editor::EditorCache,
}

#[wasm_bindgen]
impl ParsedLedger {
    /// Create a new `ParsedLedger` from a single source string.
    ///
    /// Parses, books, and validates the source. Call `isValid()` to check for errors.
    #[wasm_bindgen(constructor)]
    pub fn new(source: &str) -> Self {
        let load = load_and_book(source);
        let validation_errors = run_validation(&load);
        let editor_cache = editor::EditorCache::new(source, &load.parse_result);

        Self {
            source: source.to_string(),
            parse_result: load.parse_result,
            directives: load.directives,
            options: load.options,
            parse_errors: load.errors,
            validation_errors,
            editor_cache,
        }
    }

    /// Check if the ledger is valid (no parse or validation errors).
    #[wasm_bindgen(js_name = "isValid")]
    pub fn is_valid(&self) -> bool {
        self.parse_errors.is_empty() && self.validation_errors.is_empty()
    }

    /// Get all errors (parse + validation).
    #[wasm_bindgen(js_name = "getErrors")]
    pub fn get_errors(&self) -> Result<JsValue, JsError> {
        let mut all_errors = self.parse_errors.clone();
        all_errors.extend(self.validation_errors.clone());
        to_js(&all_errors)
    }

    /// Get parse errors only.
    #[wasm_bindgen(js_name = "getParseErrors")]
    pub fn get_parse_errors(&self) -> Result<JsValue, JsError> {
        to_js(&self.parse_errors)
    }

    /// Get validation errors only.
    #[wasm_bindgen(js_name = "getValidationErrors")]
    pub fn get_validation_errors(&self) -> Result<JsValue, JsError> {
        to_js(&self.validation_errors)
    }

    /// Get the parsed directives.
    #[wasm_bindgen(js_name = "getDirectives")]
    pub fn get_directives(&self) -> Result<JsValue, JsError> {
        let directives: Vec<_> = self.directives.iter().map(directive_to_json).collect();
        to_js(&directives)
    }

    /// Get the ledger options.
    #[wasm_bindgen(js_name = "getOptions")]
    pub fn get_options(&self) -> Result<JsValue, JsError> {
        to_js(&self.options)
    }

    /// Get the number of directives.
    #[wasm_bindgen(js_name = "directiveCount")]
    pub fn directive_count(&self) -> usize {
        self.directives.len()
    }

    /// Run a BQL query on this ledger.
    #[wasm_bindgen]
    pub fn query(&self, query_str: &str) -> Result<JsValue, JsError> {
        if !self.parse_errors.is_empty() {
            let result = QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
                errors: self.parse_errors.clone(),
            };
            return to_js(&result);
        }
        execute_query(
            &self.directives,
            query_str,
            crate::helpers::account_types_from_raw(&self.parse_result.options),
        )
    }

    /// Get account balances (shorthand for query("BALANCES")).
    #[wasm_bindgen]
    pub fn balances(&self) -> Result<JsValue, JsError> {
        self.query("BALANCES")
    }

    /// Format the ledger source.
    ///
    /// Reformats the original source preserving comments, blank lines, and
    /// non-directive content with file-wide aligned columns.
    #[wasm_bindgen]
    pub fn format(&self) -> Result<JsValue, JsError> {
        use rustledger_parser::format::format_source_with_parsed;

        if !self.parse_errors.is_empty() {
            let result = FormatResult {
                formatted: None,
                errors: self.parse_errors.clone(),
            };
            return to_js(&result);
        }

        // Reuse the cached `ParseResult` we already own instead of
        // re-parsing `self.source`. Byte-identical output to
        // `format_source(&self.source)` per the parser-side
        // `format_source_with_parsed_matches_format_source` test.
        // On large ledgers loaded into a long-lived WASM session,
        // this cuts the per-format cost roughly in half.
        let formatted = format_source_with_parsed(&self.parse_result, &self.source);

        let result = FormatResult {
            formatted: Some(formatted),
            errors: Vec::new(),
        };
        to_js(&result)
    }

    /// Expand pad directives.
    #[wasm_bindgen(js_name = "expandPads")]
    pub fn expand_pads(&self) -> Result<JsValue, JsError> {
        if !self.parse_errors.is_empty() {
            let result = PadResult {
                directives: Vec::new(),
                padding_transactions: Vec::new(),
                errors: self.parse_errors.clone(),
            };
            return to_js(&result);
        }
        execute_expand_pads(&self.directives)
    }

    /// Run a native plugin on this ledger.
    #[cfg(feature = "plugins")]
    #[wasm_bindgen(js_name = "runPlugin")]
    pub fn run_plugin(&self, plugin_name: &str) -> Result<JsValue, JsError> {
        if !self.parse_errors.is_empty() {
            let result = PluginResult {
                directives: Vec::new(),
                errors: self.parse_errors.clone(),
            };
            return to_js(&result);
        }
        execute_plugin(&self.directives, plugin_name)
    }

    // =========================================================================
    // Editor Integration (LSP-like features)
    // =========================================================================

    /// Get completions at the given position.
    #[wasm_bindgen(js_name = "getCompletions")]
    pub fn get_completions(&self, line: u32, character: u32) -> Result<JsValue, JsError> {
        let result =
            editor::get_completions_cached(&self.source, line, character, &self.editor_cache);
        to_js(&result)
    }

    /// Get hover information at the given position.
    #[wasm_bindgen(js_name = "getHoverInfo")]
    pub fn get_hover_info(&self, line: u32, character: u32) -> Result<JsValue, JsError> {
        let result = editor::get_hover_info_cached(
            &self.source,
            line,
            character,
            &self.parse_result,
            &self.editor_cache,
        );
        to_js(&result)
    }

    /// Get the definition location for the symbol at the given position.
    #[wasm_bindgen(js_name = "getDefinition")]
    pub fn get_definition(&self, line: u32, character: u32) -> Result<JsValue, JsError> {
        let result = editor::get_definition_cached(
            &self.source,
            line,
            character,
            &self.parse_result,
            &self.editor_cache,
        );
        to_js(&result)
    }

    /// Get all document symbols for the outline view.
    #[wasm_bindgen(js_name = "getDocumentSymbols")]
    pub fn get_document_symbols(&self) -> Result<JsValue, JsError> {
        let result = editor::get_document_symbols_cached(&self.parse_result, &self.editor_cache);
        to_js(&result)
    }

    /// Find all references to the symbol at the given position.
    #[wasm_bindgen(js_name = "getReferences")]
    pub fn get_references(&self, line: u32, character: u32) -> Result<JsValue, JsError> {
        let result = editor::get_references_cached(
            &self.source,
            line,
            character,
            &self.parse_result,
            &self.editor_cache,
        );
        to_js(&result)
    }

    // =========================================================================
    // Serialization / Caching
    // =========================================================================

    /// Serialize this ledger to a compact binary blob (rkyv).
    ///
    /// Store the bytes in OPFS or `IndexedDB` alongside a source fingerprint
    /// (see [`crate::hash_sources`]) and restore later with [`ParsedLedger::from_cache`].
    #[wasm_bindgen]
    pub fn serialize(&self) -> Result<Vec<u8>, JsError> {
        // Clone fields into the payload. rkyv's Serialize derive requires owned
        // types; a zero-copy borrowed serializer would add significant complexity
        // for minimal gain since serialize() is called once per cache write.
        let payload = cache::ParsedLedgerPayload {
            directives: self.directives.clone(),
            options: self.options.clone(),
            parse_errors: self.parse_errors.clone(),
            validation_errors: self.validation_errors.clone(),
        };
        cache::serialize_parsed(&payload).map_err(|e| JsError::new(&e))
    }

    /// Restore a `ParsedLedger` from bytes produced by [`ParsedLedger::serialize`].
    ///
    /// The `source` parameter must be the same source text used when the cache
    /// was created; it is re-parsed (but not re-booked or re-validated) so that
    /// editor features continue to work.
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are invalid or were produced by a different
    /// library version.
    #[wasm_bindgen(js_name = "fromCache")]
    pub fn from_cache(bytes: &[u8], source: &str) -> Result<Self, JsError> {
        let mut payload = cache::deserialize_parsed(bytes).map_err(|e| JsError::new(&e))?;

        // Re-intern strings to deduplicate identical Arc<str> allocations.
        rustledger_loader::reintern_plain_directives(&mut payload.directives);

        // Re-parse source for editor spans (cheap; booking is the expensive part).
        let parse_result = rustledger_parser::parse(source);
        let editor_cache = editor::EditorCache::new(source, &parse_result);

        Ok(Self {
            source: source.to_string(),
            parse_result,
            directives: payload.directives,
            options: payload.options,
            parse_errors: payload.parse_errors,
            validation_errors: payload.validation_errors,
            editor_cache,
        })
    }
}

// =============================================================================
// Ledger: Multi-file with queries and cross-file completions
// =============================================================================

/// A fully processed multi-file ledger for queries and validation.
///
/// Use this class for ledgers that span multiple files with `include` directives.
/// Caches the processed result for efficient repeated queries.
///
/// For single-file ledgers with editor features, use [`ParsedLedger`] instead.
///
/// # Example (JavaScript)
///
/// ```javascript
/// const ledger = Ledger.fromFiles({
///     "main.beancount": 'include "accounts.beancount"\n...',
///     "accounts.beancount": "2024-01-01 open Assets:Bank USD\n..."
/// }, "main.beancount");
///
/// if (ledger.isValid()) {
///     const balances = ledger.query("BALANCES");
///     const completions = ledger.getCompletions(currentSource, line, char);
/// }
/// ```
#[wasm_bindgen(skip_typescript)]
pub struct Ledger {
    /// The booked directives from all files.
    directives: Vec<Directive>,
    /// Where each directive was written, aligned with `directives`.
    locations: Vec<Option<SourceLocationJson>>,
    /// Ledger options.
    options: LedgerOptions,
    /// Configured account-type roots (`name_*` renames) for query
    /// classification. Not part of the wire `LedgerOptions` (deliberate);
    /// persisted in the cache payload so `fromCache` ledgers classify
    /// identically.
    account_types: rustledger_core::AccountTypes,
    /// Processing errors (load, booking, validation).
    errors: Vec<Error>,
    /// Editor cache for cross-file completions.
    editor_cache: editor::EditorCache,
}

#[wasm_bindgen]
impl Ledger {
    /// Create a `Ledger` from multiple files with include resolution.
    ///
    /// Loads and runs the same processing pipeline as the CLI:
    /// sort → synth-plugins → Early validation → book → regular-plugins → Late validation → finalize.
    ///
    /// # Arguments
    ///
    /// * `files` - A JavaScript object mapping file paths to their contents.
    /// * `entry_point` - The main file to start loading from (must exist in `files`).
    #[wasm_bindgen(js_name = "fromFiles")]
    pub fn from_files(files: JsValue, entry_point: &str) -> Result<Self, JsError> {
        use rustledger_loader::{FileSystem, LoadOptions, Loader, VirtualFileSystem, process};

        let file_map: HashMap<String, String> = serde_wasm_bindgen::from_value(files)
            .map_err(|e| JsError::new(&format!("Invalid files object: {e}")))?;

        if file_map.is_empty() {
            return Err(JsError::new("Files map cannot be empty"));
        }

        let vfs = VirtualFileSystem::from_files(file_map);

        if !vfs.exists(Path::new(entry_point)) {
            return Err(JsError::new(&format!(
                "Entry point '{entry_point}' not found in files map"
            )));
        }

        let mut loader = Loader::new().with_filesystem(Box::new(vfs));

        let load_result = match loader.load(Path::new(entry_point)) {
            Ok(result) => result,
            Err(e) => {
                return Ok(Self {
                    directives: Vec::new(),
                    locations: Vec::new(),
                    options: LedgerOptions::default(),
                    account_types: rustledger_core::AccountTypes::default(),
                    errors: vec![Error::new(format!("Load error: {e}"))],
                    editor_cache: editor::EditorCache::from_directives(&[]),
                });
            }
        };

        let options = LedgerOptions {
            title: load_result.options.title.clone(),
            operating_currencies: load_result.options.operating_currency.clone(),
        };
        let account_types = load_result.options.to_account_types();

        let load_options = LoadOptions {
            validate: true,
            ..Default::default()
        };

        // Take the load errors in full before `process` consumes the
        // `LoadResult` and flattens them — see `with_detailed_load_errors`.
        let load_errors = crate::api::load_errors_to_errors(&load_result);

        match process(load_result, &load_options) {
            Ok(ledger) => {
                // Read the locations before the `Spanned` wrappers are
                // unwrapped; the source map goes out of scope with `ledger`.
                let locations = locations_of(&ledger.directives, &ledger.source_map);
                let directives: Vec<Directive> =
                    ledger.directives.into_iter().map(|s| s.value).collect();
                let mut errors = crate::api::with_detailed_load_errors(load_errors, ledger.errors);
                // Include option warnings (E7001–E7006) so WASM consumers
                // see the same diagnostics as `rledger check` and the LSP.
                for w in &ledger.options.warnings {
                    errors.push(Error::new(format!("[{}] {}", w.code, w.message)));
                }
                let editor_cache = editor::EditorCache::from_directives(&directives);

                Ok(Self {
                    directives,
                    locations,
                    options,
                    account_types,
                    errors,
                    editor_cache,
                })
            }
            Err(e) => {
                // The load errors are what usually explains a failed
                // processing run, so keep them ahead of it.
                let mut errors = load_errors;
                errors.push(Error::new(format!("Processing error: {e}")));
                Ok(Self {
                    directives: Vec::new(),
                    locations: Vec::new(),
                    options,
                    account_types,
                    errors,
                    editor_cache: editor::EditorCache::from_directives(&[]),
                })
            }
        }
    }

    /// Check if the ledger is valid (no errors).
    #[wasm_bindgen(js_name = "isValid")]
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Get all errors.
    #[wasm_bindgen(js_name = "getErrors")]
    pub fn get_errors(&self) -> Result<JsValue, JsError> {
        to_js(&self.errors)
    }

    /// Get the parsed directives, each carrying where it was written.
    #[wasm_bindgen(js_name = "getDirectives")]
    pub fn get_directives(&self) -> Result<JsValue, JsError> {
        let directives: Vec<_> = self
            .directives
            .iter()
            .enumerate()
            .map(|(index, directive)| {
                // `get` rather than an index: a hand-built cache blob can
                // carry fewer locations than directives, and a missing one
                // means "unknown", not a panic.
                directive_to_json_at(directive, self.locations.get(index).cloned().flatten())
            })
            .collect();
        to_js(&directives)
    }

    /// Get the ledger options.
    #[wasm_bindgen(js_name = "getOptions")]
    pub fn get_options(&self) -> Result<JsValue, JsError> {
        to_js(&self.options)
    }

    /// Get the number of directives.
    #[wasm_bindgen(js_name = "directiveCount")]
    pub fn directive_count(&self) -> usize {
        self.directives.len()
    }

    /// Run a BQL query on this ledger.
    #[wasm_bindgen]
    pub fn query(&self, query_str: &str) -> Result<JsValue, JsError> {
        execute_query(&self.directives, query_str, self.account_types.clone())
    }

    /// Get account balances (shorthand for query("BALANCES")).
    #[wasm_bindgen]
    pub fn balances(&self) -> Result<JsValue, JsError> {
        self.query("BALANCES")
    }

    /// Expand pad directives.
    #[wasm_bindgen(js_name = "expandPads")]
    pub fn expand_pads(&self) -> Result<JsValue, JsError> {
        execute_expand_pads(&self.directives)
    }

    /// Run a native plugin on this ledger.
    #[cfg(feature = "plugins")]
    #[wasm_bindgen(js_name = "runPlugin")]
    pub fn run_plugin(&self, plugin_name: &str) -> Result<JsValue, JsError> {
        execute_plugin(&self.directives, plugin_name)
    }

    /// Get completions for a source string using cross-file data.
    ///
    /// Pass the source text of the file currently being edited.
    /// Completions use accounts, currencies, and payees from all loaded files.
    #[wasm_bindgen(js_name = "getCompletions")]
    pub fn get_completions(
        &self,
        source: &str,
        line: u32,
        character: u32,
    ) -> Result<JsValue, JsError> {
        let result = editor::get_completions_cached(source, line, character, &self.editor_cache);
        to_js(&result)
    }

    // =========================================================================
    // Serialization / Caching
    // =========================================================================

    /// Serialize this ledger to a compact binary blob (rkyv).
    ///
    /// Store the bytes in OPFS or `IndexedDB` alongside a source fingerprint
    /// (see [`crate::hash_sources`]) and restore later with [`Ledger::from_cache`].
    #[wasm_bindgen]
    pub fn serialize(&self) -> Result<Vec<u8>, JsError> {
        let payload = cache::LedgerPayload {
            directives: self.directives.clone(),
            locations: self.locations.clone(),
            options: self.options.clone(),
            account_type_names: vec![
                self.account_types.assets.clone(),
                self.account_types.liabilities.clone(),
                self.account_types.equity.clone(),
                self.account_types.income.clone(),
                self.account_types.expenses.clone(),
            ],
            errors: self.errors.clone(),
        };
        cache::serialize_ledger(&payload).map_err(|e| JsError::new(&e))
    }

    /// Restore a `Ledger` from bytes produced by [`Ledger::serialize`].
    ///
    /// # Errors
    ///
    /// Returns an error if the bytes are invalid or were produced by a different
    /// library version.
    #[wasm_bindgen(js_name = "fromCache")]
    pub fn from_cache(bytes: &[u8]) -> Result<Self, JsError> {
        let mut payload = cache::deserialize_ledger(bytes).map_err(|e| JsError::new(&e))?;

        // Re-intern strings to deduplicate identical Arc<str> allocations.
        rustledger_loader::reintern_plain_directives(&mut payload.directives);

        let editor_cache = editor::EditorCache::from_directives(&payload.directives);

        let account_types = match <[String; 5]>::try_from(payload.account_type_names) {
            Ok([assets, liabilities, equity, income, expenses]) => rustledger_core::AccountTypes {
                assets,
                liabilities,
                equity,
                income,
                expenses,
            },
            // Wrong arity can only come from a hand-built blob (the version
            // header already gates format changes); fall back to defaults
            // rather than erroring on an otherwise-valid payload.
            Err(_) => rustledger_core::AccountTypes::default(),
        };

        Ok(Self {
            directives: payload.directives,
            locations: payload.locations,
            options: payload.options,
            account_types,
            errors: payload.errors,
            editor_cache,
        })
    }
}

// Host-only: `Ledger::from_files` takes a `JsValue`, so the multi-file class
// itself cannot be built off-target. `locations_of` is the part that reads
// the loader's source map, and it takes plain loader types.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use rustledger_loader::{LoadOptions, Loader, VirtualFileSystem, process};

    /// Read the files the way `Ledger::from_files` does, up to `process`.
    fn read(files: &[(&str, &str)], entry_point: &str) -> rustledger_loader::LoadResult {
        let mut vfs = VirtualFileSystem::new();
        for (path, source) in files {
            vfs.add_file(*path, *source);
        }
        Loader::new()
            .with_filesystem(Box::new(vfs))
            .load(Path::new(entry_point))
            .expect("the ledger loads")
    }

    /// Load a ledger the way `Ledger::from_files` does.
    fn load(files: &[(&str, &str)], entry_point: &str) -> rustledger_loader::Ledger {
        process(
            read(files, entry_point),
            &LoadOptions {
                validate: true,
                ..Default::default()
            },
        )
        .expect("the ledger processes")
    }

    /// The errors `Ledger::from_files` reports, built the way it builds them.
    fn errors_of(files: &[(&str, &str)], entry_point: &str) -> Vec<Error> {
        let raw = read(files, entry_point);
        let load_errors = crate::api::load_errors_to_errors(&raw);
        let ledger = process(
            raw,
            &LoadOptions {
                validate: true,
                ..Default::default()
            },
        )
        .expect("the ledger processes");

        crate::api::with_detailed_load_errors(load_errors, ledger.errors)
    }

    #[test]
    fn directives_carry_the_file_and_lines_they_were_written_on() {
        let main = "\
option \"operating_currency\" \"JPY\"
include \"sub.beancount\"

2025-01-10 open Assets:Cash JPY
2025-01-10 open Expenses:Food JPY

2025-01-28 * \"Groceries\"
  Expenses:Food   4200 JPY
  Assets:Cash
";
        let sub = "\
2025-02-14 * \"Lunch\"
  Expenses:Food    780 JPY
  Assets:Cash
";
        let ledger = load(
            &[("main.beancount", main), ("sub.beancount", sub)],
            "main.beancount",
        );

        let located: Vec<_> = locations_of(&ledger.directives, &ledger.source_map)
            .into_iter()
            .map(|location| {
                let location = location.expect("every directive here was written in a file");
                (location.file, location.line, location.end_line)
            })
            .collect();

        assert_eq!(
            located,
            vec![
                ("main.beancount".to_string(), 4, 4),
                // The blank line after it is a separator, not part of the open.
                ("main.beancount".to_string(), 5, 5),
                // A transaction runs from its header through its last posting.
                ("main.beancount".to_string(), 7, 9),
                // Included files are located in their own file, not the entry point.
                ("sub.beancount".to_string(), 1, 3),
            ]
        );
    }

    #[test]
    fn a_parse_error_says_which_file_and_line_it_is_on() {
        // A tag with non-ASCII letters, which the parser rejects: the kind of
        // typo someone makes while editing, in an included file.
        let main = "\
option \"operating_currency\" \"JPY\"
include \"sub.beancount\"

2025-01-10 open Assets:Cash JPY
2025-01-10 open Expenses:Food JPY
";
        let sub = "\
2025-02-14 * \"Lunch\" #日用品
  Expenses:Food    780 JPY
  Assets:Cash
";

        let errors = errors_of(
            &[("main.beancount", main), ("sub.beancount", sub)],
            "main.beancount",
        );

        // The editor can be taken to the tag, the way `rledger check` points
        // at it — not merely told that some file somewhere failed to parse.
        let parse_errors: Vec<_> = errors
            .iter()
            .filter(|error| error.phase.as_deref() == Some("parse"))
            .collect();
        assert!(!parse_errors.is_empty(), "the tag is rejected");
        for error in &parse_errors {
            assert_eq!(error.file.as_deref(), Some("sub.beancount"));
            assert_eq!(error.line, Some(1));
            assert_eq!(error.code.as_deref(), Some("P0012"));
            assert!(error.column.is_some(), "the column is kept too: {error:?}");
        }

        // And the flattened "parse errors in <file>" no longer stands in for it.
        assert!(
            errors
                .iter()
                .all(|error| error.code.as_deref() != Some("LOAD"))
        );
    }

    #[test]
    fn a_directive_a_plugin_synthesized_has_no_location() {
        let main = "\
plugin \"beancount.plugins.auto_accounts\"

2025-01-28 * \"Groceries\"
  Expenses:Food   4200 JPY
  Assets:Cash
";
        let ledger = load(&[("main.beancount", main)], "main.beancount");
        let locations = locations_of(&ledger.directives, &ledger.source_map);

        // The plugin opens the two accounts the transaction uses; those Opens
        // are nowhere in the text, so they have no place to point at.
        let synthesized = ledger
            .directives
            .iter()
            .zip(&locations)
            .filter(|(directive, _)| matches!(directive.value, Directive::Open(_)))
            .collect::<Vec<_>>();
        assert_eq!(synthesized.len(), 2);
        assert!(synthesized.iter().all(|(_, location)| location.is_none()));

        // The transaction that is in the text still knows where it is.
        let written = locations.last().expect("the transaction is last");
        let written = written.as_ref().expect("it was written in the file");
        assert_eq!((written.line, written.end_line), (3, 5));
    }
}
