use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::num::NonZero;
use std::path::Path;

use log::warn;
use lru::LruCache;
use lsp_types::{SemanticToken, SemanticTokens, SemanticTokensResult, Url};
use shader_sense::symbols::symbols::ShaderSymbolMode;
use shader_sense::{
    position::{ShaderPosition, ShaderRange},
    symbols::{
        symbol_list::ShaderSymbolListRef,
        symbols::{ShaderSymbol, ShaderSymbolData},
    },
};

use crate::server::common::ServerLanguageError;
use crate::server::ServerLanguage;

const TOKEN_TYPE_MACRO: u32 = 0;
const TOKEN_TYPE_PARAMETER: u32 = 1;
const TOKEN_TYPE_ENUM_MEMBER: u32 = 2;
const TOKEN_TYPE_ENUM: u32 = 3;
const TOKEN_TYPE_VARIABLE: u32 = 4;
const TOKEN_TYPE_FUNCTION: u32 = 5;
const TOKEN_TYPE_TYPE: u32 = 6;
const TOKEN_TYPE_PROPERTY: u32 = 7;

#[derive(Clone, Copy, Debug)]
struct AbsoluteSemanticToken {
    line: u32,
    start: u32,
    length: u32,
    token_type: u32,
    token_modifiers_bitset: u32,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SymbolDeclarationKey {
    label: String,
    start_line: u32,
    start_pos: u32,
    end_line: u32,
    end_pos: u32,
}

impl AbsoluteSemanticToken {
    fn new(line: u32, start: u32, length: u32, token_type: u32) -> Self {
        Self {
            line,
            start,
            length,
            token_type,
            token_modifiers_bitset: 0,
        }
    }

    fn from_semantic_token(token: SemanticToken) -> Self {
        Self {
            line: token.delta_line,
            start: token.delta_start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers_bitset: token.token_modifiers_bitset,
        }
    }
}

impl ServerLanguage {
    fn token_precedence(token_type: u32) -> u8 {
        match token_type {
            TOKEN_TYPE_MACRO => 8,
            TOKEN_TYPE_ENUM_MEMBER => 7,
            TOKEN_TYPE_PARAMETER => 6,
            TOKEN_TYPE_PROPERTY => 5,
            TOKEN_TYPE_FUNCTION => 4,
            TOKEN_TYPE_ENUM => 3,
            TOKEN_TYPE_TYPE => 2,
            TOKEN_TYPE_VARIABLE => 1,
            _ => 0,
        }
    }

    fn normalize_tokens(tokens: Vec<SemanticToken>) -> Vec<AbsoluteSemanticToken> {
        tokens
            .into_iter()
            .map(AbsoluteSemanticToken::from_semantic_token)
            .collect()
    }

    fn merge_tokens(tokens: Vec<AbsoluteSemanticToken>) -> Vec<AbsoluteSemanticToken> {
        let mut unique_tokens: HashMap<(u32, u32, u32), AbsoluteSemanticToken> = HashMap::new();
        for token in tokens {
            unique_tokens
                .entry((token.line, token.start, token.length))
                .and_modify(|existing| {
                    if Self::token_precedence(token.token_type)
                        > Self::token_precedence(existing.token_type)
                    {
                        *existing = token;
                    }
                })
                .or_insert(token);
        }

        let mut tokens = unique_tokens.into_values().collect::<Vec<_>>();
        tokens.sort_by(|lhs, rhs| (&lhs.line, &lhs.start).cmp(&(&rhs.line, &rhs.start)));
        tokens
    }

    fn encode_tokens(mut tokens: Vec<AbsoluteSemanticToken>) -> Vec<SemanticToken> {
        let mut delta_line = 0;
        let mut delta_pos = 0;
        for token in &mut tokens {
            if token.line != delta_line {
                delta_pos = 0;
            }
            let line = token.line;
            let pos = token.start;
            token.line = line - delta_line;
            token.start = pos - delta_pos;
            delta_line = line;
            delta_pos = pos;
        }

        tokens
            .into_iter()
            .map(|token| SemanticToken {
                delta_line: token.line,
                delta_start: token.start,
                length: token.length,
                token_type: token.token_type,
                token_modifiers_bitset: token.token_modifiers_bitset,
            })
            .collect()
    }

    fn push_range_token(
        tokens: &mut Vec<AbsoluteSemanticToken>,
        range: &ShaderRange,
        label_len: usize,
        token_type: u32,
    ) {
        tokens.push(AbsoluteSemanticToken::new(
            range.start.line,
            range.start.pos,
            label_len as u32,
            token_type,
        ));
    }

    fn declaration_key(label: &str, range: &ShaderRange) -> SymbolDeclarationKey {
        SymbolDeclarationKey {
            label: label.to_owned(),
            start_line: range.start.line,
            start_pos: range.start.pos,
            end_line: range.end.line,
            end_pos: range.end.pos,
        }
    }

    fn collect_parameter_declaration_keys(
        symbols: &ShaderSymbolListRef,
        current_file: &Path,
    ) -> HashSet<SymbolDeclarationKey> {
        symbols
            .functions
            .iter()
            .filter_map(|symbol| {
                let runtime = symbol.mode.map_runtime()?;
                if runtime.file_path.as_os_str() != current_file.as_os_str() {
                    return None;
                }
                match &symbol.data {
                    ShaderSymbolData::Functions { signatures } => Some(
                        signatures
                            .iter()
                            .flat_map(|signature| signature.parameters.iter())
                            .filter_map(|parameter| {
                                parameter
                                    .range
                                    .as_ref()
                                    .map(|range| Self::declaration_key(&parameter.label, range))
                            })
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                }
            })
            .flatten()
            .collect()
    }

    fn is_parameter_symbol(
        symbol: &ShaderSymbol,
        current_file: &Path,
        parameter_keys: &HashSet<SymbolDeclarationKey>,
    ) -> bool {
        match symbol.mode.map_runtime() {
            Some(runtime) if runtime.file_path.as_os_str() == current_file.as_os_str() => {
                parameter_keys.contains(&Self::declaration_key(&symbol.label, &runtime.range))
            }
            _ => false,
        }
    }

    fn find_symbol_declaration_tokens(&mut self, uri: &Url) -> Vec<AbsoluteSemanticToken> {
        let symbols = self.watched_files.get_all_symbols(uri);
        let file_path = uri.to_file_path().unwrap();
        let parameter_keys = Self::collect_parameter_declaration_keys(&symbols, &file_path);
        let mut tokens = Vec::new();

        for symbol in &symbols.variables {
            if let Some(runtime) = symbol.mode.map_runtime() {
                if runtime.file_path.as_os_str() == file_path.as_os_str() {
                    let token_type =
                        if Self::is_parameter_symbol(symbol, &file_path, &parameter_keys) {
                            TOKEN_TYPE_PARAMETER
                        } else {
                            TOKEN_TYPE_VARIABLE
                        };
                    Self::push_range_token(
                        &mut tokens,
                        &runtime.range,
                        symbol.label.len(),
                        token_type,
                    );
                }
            }
        }

        for symbol in &symbols.functions {
            if let Some(runtime) = symbol.mode.map_runtime() {
                if runtime.file_path.as_os_str() == file_path.as_os_str() {
                    Self::push_range_token(
                        &mut tokens,
                        &runtime.range,
                        symbol.label.len(),
                        TOKEN_TYPE_FUNCTION,
                    );
                }
            }
        }

        for symbol in &symbols.types {
            if let Some(runtime) = symbol.mode.map_runtime() {
                if runtime.file_path.as_os_str() == file_path.as_os_str() {
                    let token_type = match &symbol.data {
                        ShaderSymbolData::Enum { .. } => TOKEN_TYPE_ENUM,
                        _ => TOKEN_TYPE_TYPE,
                    };
                    Self::push_range_token(
                        &mut tokens,
                        &runtime.range,
                        symbol.label.len(),
                        token_type,
                    );

                    match &symbol.data {
                        ShaderSymbolData::Struct {
                            constructors: _,
                            members,
                            methods,
                        } => {
                            for member in members {
                                if let Some(range) = &member.parameters.range {
                                    Self::push_range_token(
                                        &mut tokens,
                                        range,
                                        member.parameters.label.len(),
                                        TOKEN_TYPE_PROPERTY,
                                    );
                                }
                            }
                            for method in methods {
                                if let Some(range) = &method.range {
                                    Self::push_range_token(
                                        &mut tokens,
                                        range,
                                        method.label.len(),
                                        TOKEN_TYPE_FUNCTION,
                                    );
                                }
                            }
                        }
                        ShaderSymbolData::Enum { values } => {
                            for value in values {
                                if let Some(range) = &value.range {
                                    Self::push_range_token(
                                        &mut tokens,
                                        range,
                                        value.label.len(),
                                        TOKEN_TYPE_ENUM_MEMBER,
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        tokens
    }

    fn classify_word_symbol(
        is_field: bool,
        resolved_symbols: &[ShaderSymbol],
        symbols: &ShaderSymbolListRef,
        current_file: &Path,
        parameter_keys: &HashSet<SymbolDeclarationKey>,
    ) -> Option<u32> {
        for symbol in resolved_symbols {
            match &symbol.data {
                ShaderSymbolData::Macro { .. } => return Some(TOKEN_TYPE_MACRO),
                ShaderSymbolData::CallExpression { .. }
                | ShaderSymbolData::Functions { .. }
                | ShaderSymbolData::Method { .. } => return Some(TOKEN_TYPE_FUNCTION),
                ShaderSymbolData::Enum { .. } => return Some(TOKEN_TYPE_ENUM),
                ShaderSymbolData::Types { .. } | ShaderSymbolData::Struct { .. } => {
                    return Some(TOKEN_TYPE_TYPE)
                }
                ShaderSymbolData::Parameter { context, .. } => {
                    if is_field {
                        return match symbols.find_type_symbol(context) {
                            Some(enum_symbol)
                                if matches!(enum_symbol.data, ShaderSymbolData::Enum { .. }) =>
                            {
                                Some(TOKEN_TYPE_ENUM_MEMBER)
                            }
                            _ => Some(TOKEN_TYPE_PROPERTY),
                        };
                    }
                }
                ShaderSymbolData::Variables { .. } => {
                    if Self::is_parameter_symbol(symbol, current_file, parameter_keys) {
                        return Some(TOKEN_TYPE_PARAMETER);
                    }
                    return Some(TOKEN_TYPE_VARIABLE);
                }
                ShaderSymbolData::Constants { .. } => return Some(TOKEN_TYPE_VARIABLE),
                ShaderSymbolData::Include { .. } | ShaderSymbolData::Keyword { .. } => {}
            }
        }

        None
    }

    fn find_identifier_symbol_tokens(&mut self, uri: &Url) -> Vec<AbsoluteSemanticToken> {
        let cached_file = self.watched_files.files.get(uri).unwrap();
        let language_data = self
            .language_data
            .get(&cached_file.shading_language)
            .unwrap();
        let symbols = self.watched_files.get_all_symbols(uri);
        let file_path = uri.to_file_path().unwrap();
        let parameter_keys = Self::collect_parameter_declaration_keys(&symbols, &file_path);
        let shader_module = RefCell::borrow(&cached_file.shader_module);
        let content = &shader_module.content;
        let identifier_regex = regex::Regex::new(r"\b[A-Za-z_][A-Za-z0-9_]*\b").unwrap();

        identifier_regex
            .find_iter(content)
            .filter_map(|identifier| {
                let position =
                    ShaderPosition::from_byte_offset(content, identifier.start()).ok()?;
                let word_range = language_data
                    .symbol_provider
                    .get_word_range_at_position(&shader_module, &position)
                    .ok()?;
                if word_range.get_range().start.line != position.line
                    || word_range.get_range().start.pos != position.pos
                {
                    return None;
                }

                let resolved_symbols =
                    word_range.find_symbol_from_parent(file_path.clone(), &symbols);
                let token_type = Self::classify_word_symbol(
                    word_range.is_field(),
                    &resolved_symbols,
                    &symbols,
                    &file_path,
                    &parameter_keys,
                )?;
                Some(AbsoluteSemanticToken::new(
                    position.line,
                    position.pos,
                    identifier.as_str().len() as u32,
                    token_type,
                ))
            })
            .collect()
    }

    fn get_regex<'a>(
        label: &String,
        regex_cache: &'a mut LruCache<String, regex::Regex>,
    ) -> &'a regex::Regex {
        regex_cache.get_or_insert(label.clone(), || {
            regex::Regex::new(format!("\\b({})\\b", regex::escape(label)).as_str()).unwrap()
        })
    }
    fn find_macros(&mut self, uri: &Url) -> Vec<SemanticToken> {
        let cached_file = self.watched_files.files.get(uri).unwrap();
        let symbols = self.watched_files.get_all_symbols(&uri);
        let file_path = uri.to_file_path().unwrap();
        let content = &RefCell::borrow(&cached_file.shader_module).content;
        symbols
            .macros
            .iter()
            .map(|symbol| {
                let byte_offset_start = match &symbol.mode {
                    ShaderSymbolMode::Runtime(runtime) => {
                        if runtime.file_path.as_os_str() == file_path.as_os_str() {
                            runtime.range.start.to_byte_offset(content).unwrap()
                        } else {
                            match cached_file
                                .data
                                .as_ref()
                                .unwrap()
                                .symbol_cache
                                .find_direct_includer(&runtime.file_path)
                            {
                                Some(include) => {
                                    include.get_range().start.to_byte_offset(content).unwrap()
                                }
                                None => 0, // Included from another file, but not found...
                            }
                        }
                    }
                    _ => 0, // Not runtime means no range means its everywhere
                };
                // Need to ignore comment aswell... Might need tree sitter instead.
                // Looking for preproc_arg & identifier might be enough.
                // Need to check for regions too...
                let reg = Self::get_regex(&symbol.label, &mut self.regex_cache);
                let word_byte_offsets: Vec<usize> = reg
                    .captures_iter(&content[byte_offset_start..])
                    .map(|e| e.get(0).unwrap().range().start + byte_offset_start)
                    .collect();
                word_byte_offsets
                    .iter()
                    .filter_map(|byte_offset| {
                        match ShaderPosition::from_byte_offset(&content, *byte_offset) {
                            Ok(position) => Some(SemanticToken {
                                delta_line: position.line,
                                delta_start: position.pos,
                                length: symbol.label.len() as u32,
                                token_type: TOKEN_TYPE_MACRO,
                                token_modifiers_bitset: 0,
                            }),
                            Err(_) => None,
                        }
                    })
                    .collect()
            })
            .collect::<Vec<Vec<SemanticToken>>>()
            .concat()
    }
    fn find_enum(&mut self, uri: &Url) -> Vec<SemanticToken> {
        let cached_file = self.watched_files.files.get(uri).unwrap();
        let symbols = self.watched_files.get_all_symbols(&uri);
        let file_path = uri.to_file_path().unwrap();
        let content = &RefCell::borrow(&cached_file.shader_module).content;
        symbols
            .types
            .iter()
            .filter_map(|symbol| match &symbol.data {
                ShaderSymbolData::Enum { values } => {
                    let byte_offset_start = match &symbol.mode {
                        ShaderSymbolMode::Runtime(runtime) => {
                            if runtime.file_path.as_os_str() == file_path.as_os_str() {
                                runtime.range.start.to_byte_offset(content).unwrap()
                            } else {
                                match cached_file
                                    .data
                                    .as_ref()
                                    .unwrap()
                                    .symbol_cache
                                    .find_direct_includer(&runtime.file_path)
                                {
                                    Some(include) => {
                                        include.get_range().start.to_byte_offset(content).unwrap()
                                    }
                                    None => 0, // Included from another file, but not found...
                                }
                            }
                        }
                        _ => 0, // Not runtime means no range means its everywhere
                    };
                    let mut tokens = Vec::new();
                    // Add enum label aswell.
                    let reg = Self::get_regex(&symbol.label, &mut self.regex_cache);
                    let word_byte_offsets: Vec<usize> = reg
                        .captures_iter(&content[byte_offset_start..])
                        .map(|c| c.get(0).unwrap().range().start + byte_offset_start)
                        .collect();
                    tokens.extend(
                        word_byte_offsets
                            .iter()
                            .filter_map(|byte_offset| {
                                match ShaderPosition::from_byte_offset(&content, *byte_offset) {
                                    Ok(position) => Some(SemanticToken {
                                        delta_line: position.line,
                                        delta_start: position.pos,
                                        length: symbol.label.len() as u32,
                                        token_type: TOKEN_TYPE_ENUM,
                                        token_modifiers_bitset: 0,
                                    }),
                                    Err(_) => None,
                                }
                            })
                            .collect::<Vec<SemanticToken>>(),
                    );
                    // Collect enum member now.
                    for value in values {
                        let reg = Self::get_regex(&value.label, &mut self.regex_cache);
                        let word_byte_offsets: Vec<usize> = reg
                            .captures_iter(&content[byte_offset_start..])
                            .map(|e| e.get(0).unwrap().range().start + byte_offset_start)
                            .collect();
                        tokens.extend(
                            word_byte_offsets
                                .iter()
                                .filter_map(|byte_offset| {
                                    match ShaderPosition::from_byte_offset(&content, *byte_offset) {
                                        Ok(position) => Some(SemanticToken {
                                            delta_line: position.line,
                                            delta_start: position.pos,
                                            length: value.label.len() as u32,
                                            token_type: TOKEN_TYPE_ENUM_MEMBER,
                                            token_modifiers_bitset: 0,
                                        }),
                                        Err(_) => None,
                                    }
                                })
                                .collect::<Vec<SemanticToken>>(),
                        );
                    }
                    Some(tokens)
                }
                _ => None,
            })
            .collect::<Vec<Vec<SemanticToken>>>()
            .concat()
    }
    pub fn recolt_semantic_tokens(
        &mut self,
        uri: &Url,
    ) -> Result<SemanticTokensResult, ServerLanguageError> {
        // Ensure valid file input.
        let _cached_file = self.get_cachable_file(&uri)?;
        // Find occurences of tokens to paint
        let mut tokens = Vec::new();
        tokens.extend(Self::normalize_tokens(self.find_macros(uri)));
        tokens.extend(Self::normalize_tokens(self.find_enum(uri)));
        tokens.extend(self.find_symbol_declaration_tokens(uri));
        tokens.extend(self.find_identifier_symbol_tokens(uri));
        let tokens = Self::merge_tokens(tokens);

        // Increase cache size if we couldnt fit all tokens.
        if tokens.len() > self.regex_cache.cap().get() {
            let scale_factor = 1.2; // Allocate a bit more than the max.
            let max_size = 2000; // Avoid using too much memory.
            if tokens.len() <= max_size {
                let new_cap =
                    std::cmp::min((tokens.len() as f32 * scale_factor) as usize, max_size);
                warn!(
                    "Too many tokens found for single file {} ({}). Extending regex cache size to {}.",
                    uri,
                    tokens.len(),
                    new_cap
                );
                let old_cache = std::mem::replace(
                    &mut self.regex_cache,
                    LruCache::new(NonZero::new(new_cap).unwrap()),
                );
                for (old_key, old_regex) in old_cache {
                    self.regex_cache.put(old_key, old_regex);
                }
            } else {
                warn!(
                    "Too many tokens found for single file {} ({}), maximum limit {} reached.",
                    uri,
                    tokens.len(),
                    max_size
                );
            }
        }
        Ok(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: Self::encode_tokens(tokens),
        }))
    }
}
