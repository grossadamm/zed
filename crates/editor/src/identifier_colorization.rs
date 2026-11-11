//! Colors identifiers by their name, also known as "semantic highlighting" or
//! "hashed syntax highlighting" in other editors.
//! Uses the tree-sitter captures from highlights.scm to find the identifiers,
//! so it needs no language server.

use std::ops::Range;
use std::sync::Arc;

use collections::{HashMap, HashSet};
use gpui::{AppContext as _, Context, HighlightStyle, Rgba};
use language::{BufferSnapshot, LanguageName, language_settings::LanguageSettings};
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use text::{BufferId, OffsetRangeExt as _, Point};

use crate::{
    Editor, HighlightKey,
    semantic_tokens::{color_at_position, name_color_position},
};

/// Identifiers are computed for whole chunks of this many buffer rows, and each chunk is
/// computed once until the buffer changes. A chunk is taller than a typical viewport, so
/// rows next to the visible ones are already colored when they are scrolled into view.
const ROWS_PER_CHUNK: u32 = 128;

/// Chunks with more text than this are left uncolored. They come from minified and
/// generated files, where a single chunk can hold hundreds of thousands of names.
const MAX_BYTES_PER_CHUNK: usize = 256 * 1024;

/// The first segment of the highlights.scm capture names that are colored by name.
const COLORED_CAPTURE_KINDS: &[&str] = &[
    "variable",
    "property",
    "function",
    "type",
    "constant",
    "attribute",
    "namespace",
    "module",
];

/// JSX component names are captured as tags, but they are the names of functions and
/// classes, unlike the fixed tag names of HTML.
const COMPONENT_TAG_CAPTURE: &str = "tag.component";

/// Characters that are never part of a name. Grammars also capture paths, quoted keys
/// and whole type expressions with the colored kinds.
const NON_NAME_CHARACTERS: &[char] = &[
    '/', '\\', '(', ')', '[', ']', '{', '}', '<', '>', '|', ',', ';', '=', '"', '`',
];

struct ExcerptIdentifiers {
    buffer_snapshot: BufferSnapshot,
    excerpt_context: Range<text::Anchor>,
    chunks: Vec<u32>,
    colors: Arc<[Rgba]>,
    color_builtins: bool,
    default_library_ranges: Arc<[Range<Anchor>]>,
}

impl Editor {
    pub(crate) fn colorize_identifiers(&mut self, invalidate: bool, cx: &mut Context<Editor>) {
        if !self.mode.is_full() {
            return;
        }

        if invalidate {
            self.colorized_identifier_chunks.clear();
            self.identifier_colors_stale = true;
        }

        let excerpts = self.excerpts_to_colorize(cx);
        if excerpts.is_empty() {
            if std::mem::take(&mut self.identifier_colors_stale) {
                self.colorize_identifiers_task = gpui::Task::ready(());
                self.clear_highlights_with(
                    &mut |key| matches!(key, HighlightKey::ColorizeIdentifier(_)),
                    cx,
                );
            }
            return;
        }

        let multi_buffer_snapshot = self.buffer().read(cx).snapshot(cx);
        let colorized = cx.background_spawn(async move {
            let mut ranges_by_color = HashMap::<u32, (Rgba, Vec<Range<Anchor>>)>::default();
            let mut colorized_chunks = Vec::new();
            for excerpt in excerpts {
                colorize_excerpt(&multi_buffer_snapshot, &excerpt, &mut ranges_by_color);
                colorized_chunks.push((excerpt.excerpt_context, excerpt.chunks));
            }
            (ranges_by_color, colorized_chunks)
        });

        self.colorize_identifiers_task = cx.spawn(async move |editor, cx| {
            let (ranges_by_color, colorized_chunks) = colorized.await;
            editor
                .update(cx, |editor, cx| {
                    let clear_previous = std::mem::take(&mut editor.identifier_colors_stale);
                    editor.display_map.update(cx, |display_map, cx| {
                        if clear_previous {
                            display_map.clear_highlights_with(&mut |key| {
                                matches!(key, HighlightKey::ColorizeIdentifier(_))
                            });
                        }
                        for (color_key, (color, ranges)) in ranges_by_color {
                            display_map.highlight_text(
                                HighlightKey::ColorizeIdentifier(color_key),
                                ranges,
                                HighlightStyle {
                                    color: Some(color.into()),
                                    ..HighlightStyle::default()
                                },
                                true,
                                cx,
                            );
                        }
                    });
                    for (excerpt_context, chunks) in colorized_chunks {
                        editor
                            .colorized_identifier_chunks
                            .entry(excerpt_context)
                            .or_default()
                            .extend(chunks);
                    }
                    cx.notify();
                })
                .ok();
        });
    }

    /// The visible excerpts that have identifier colorization enabled and chunks that are
    /// not colorized yet. This runs on every scroll, so it does the cheap cache check
    /// before anything else.
    fn excerpts_to_colorize(&mut self, cx: &mut Context<Editor>) -> Vec<ExcerptIdentifiers> {
        let mut excerpts = Vec::new();
        let mut theme_accents = None;
        let mut default_library_ranges_by_buffer =
            HashMap::<BufferId, Arc<[Range<Anchor>]>>::default();
        for (buffer_snapshot, visible_range, excerpt_range) in self.visible_buffer_ranges(cx) {
            let buffer_id = buffer_snapshot.remote_id();
            let Some(buffer) = self.buffer().read(cx).buffer(buffer_id) else {
                continue;
            };
            let (language_name, colors, color_builtins) = {
                let buffer = buffer.read(cx);
                let Some(language) = buffer
                    .language()
                    .filter(|language| language.grammar().is_some())
                else {
                    continue;
                };
                let settings = LanguageSettings::for_buffer(buffer, cx);
                if !settings.colorize_identifiers.enabled {
                    continue;
                }
                (
                    language.name(),
                    settings.colorize_identifiers.colors.clone(),
                    settings.colorize_identifiers.color_builtins,
                )
            };

            let first_chunk =
                buffer_snapshot.offset_to_point(visible_range.start.0).row / ROWS_PER_CHUNK;
            let last_chunk =
                buffer_snapshot.offset_to_point(visible_range.end.0).row / ROWS_PER_CHUNK;
            let colorized_chunks = self.colorized_identifier_chunks.get(&excerpt_range.context);
            let chunks = (first_chunk..=last_chunk)
                .filter(|chunk| {
                    !colorized_chunks.is_some_and(|colorized| colorized.contains(chunk))
                })
                .collect::<Vec<_>>();
            if chunks.is_empty() {
                continue;
            }

            let colors = if colors.is_empty() {
                theme_accents
                    .get_or_insert_with(|| self.theme_accent_colors())
                    .clone()
            } else {
                colors
            };
            if colors.is_empty() {
                continue;
            }

            let default_library_ranges = if color_builtins {
                Arc::default()
            } else if let Some(ranges) = default_library_ranges_by_buffer.get(&buffer_id) {
                ranges.clone()
            } else {
                let ranges: Arc<[Range<Anchor>]> = self
                    .default_library_token_ranges(buffer_id, &language_name, cx)
                    .into();
                default_library_ranges_by_buffer.insert(buffer_id, ranges.clone());
                ranges
            };
            excerpts.push(ExcerptIdentifiers {
                buffer_snapshot,
                excerpt_context: excerpt_range.context,
                chunks,
                colors,
                color_builtins,
                default_library_ranges,
            });
        }
        excerpts
    }

    fn theme_accent_colors(&self) -> Arc<[Rgba]> {
        self.accent_data
            .as_ref()
            .map(|accent_data| {
                accent_data
                    .colors
                    .0
                    .iter()
                    .copied()
                    .map(Rgba::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Which names a language server reports as standard library decides whether they are
    /// colored, so a change of the semantic tokens makes the identifier colors outdated.
    /// This also replaces a colorization that is still being computed from the previous
    /// semantic tokens.
    pub(crate) fn recolorize_identifiers_after_semantic_tokens_change(
        &mut self,
        cx: &mut Context<Editor>,
    ) {
        self.colorize_identifiers(true, cx);
    }

    /// The ranges of the semantic tokens that a language server marked as part of the
    /// standard library.
    fn default_library_token_ranges(
        &self,
        buffer_id: BufferId,
        language_name: &LanguageName,
        cx: &mut Context<Editor>,
    ) -> Vec<Range<Anchor>> {
        let Some(project) = self.project.clone() else {
            return Vec::new();
        };
        let Some(tokens) = self
            .display_map
            .read(cx)
            .semantic_token_highlights
            .get(&buffer_id)
            .map(|(tokens, _)| tokens.clone())
        else {
            return Vec::new();
        };

        let server_ids = tokens
            .iter()
            .map(|token| token.server_id)
            .collect::<HashSet<_>>();
        let lsp_store = project.read(cx).lsp_store();
        lsp_store.update(cx, |lsp_store, cx| {
            let mut ranges = Vec::new();
            for server_id in server_ids {
                let Some(stylizer) =
                    lsp_store.get_or_create_token_stylizer(server_id, Some(language_name), cx)
                else {
                    continue;
                };
                ranges.extend(
                    tokens
                        .iter()
                        .filter(|token| {
                            token.server_id == server_id
                                && stylizer.has_modifier(
                                    token.token_modifiers,
                                    lsp::SemanticTokenModifier::DEFAULT_LIBRARY.as_str(),
                                )
                        })
                        .map(|token| token.range.clone()),
                );
            }
            ranges
        })
    }
}

fn colorize_excerpt(
    multi_buffer_snapshot: &MultiBufferSnapshot,
    excerpt: &ExcerptIdentifiers,
    ranges_by_color: &mut HashMap<u32, (Rgba, Vec<Range<Anchor>>)>,
) {
    let buffer_snapshot = &excerpt.buffer_snapshot;
    let excerpt_context = excerpt.excerpt_context.to_offset(buffer_snapshot);
    let mut default_library_starts = excerpt
        .default_library_ranges
        .iter()
        .map(|range| range.start.to_offset(multi_buffer_snapshot))
        .collect::<Vec<MultiBufferOffset>>();
    default_library_starts.sort();

    for &chunk in &excerpt.chunks {
        let chunk_start = buffer_snapshot.point_to_offset(buffer_snapshot.clip_point(
            Point::new(chunk.saturating_mul(ROWS_PER_CHUNK), 0),
            text::Bias::Left,
        ));
        let chunk_end = buffer_snapshot.point_to_offset(buffer_snapshot.clip_point(
            Point::new((chunk + 1).saturating_mul(ROWS_PER_CHUNK), 0),
            text::Bias::Left,
        ));
        let chunk_range =
            chunk_start.max(excerpt_context.start)..chunk_end.min(excerpt_context.end);
        if chunk_range.is_empty() || chunk_range.len() > MAX_BYTES_PER_CHUNK {
            continue;
        }

        for identifier_range in
            identifier_ranges(buffer_snapshot, chunk_range, excerpt.color_builtins)
        {
            if !is_name(buffer_snapshot.text_for_range(identifier_range.clone())) {
                continue;
            }
            let Some(range) = multi_buffer_snapshot.anchor_range_in_buffer(
                buffer_snapshot.anchor_range_inside(identifier_range.clone()),
            ) else {
                continue;
            };
            if !default_library_starts.is_empty()
                && default_library_starts
                    .binary_search(&range.start.to_offset(multi_buffer_snapshot))
                    .is_ok()
            {
                continue;
            }
            let position = name_color_position(buffer_snapshot.text_for_range(identifier_range));
            let Some(color) = color_at_position(&excerpt.colors, position) else {
                continue;
            };
            ranges_by_color
                .entry(u32::from(color))
                .or_insert_with(|| (color, Vec::new()))
                .1
                .push(range);
        }
    }
}

/// Returns the byte ranges of the identifiers that start in `range`, in buffer order.
fn identifier_ranges(
    buffer_snapshot: &BufferSnapshot,
    range: Range<usize>,
    color_builtins: bool,
) -> Vec<Range<usize>> {
    let mut captures = buffer_snapshot.captures(range.clone(), |grammar| {
        grammar
            .highlights_config
            .as_ref()
            .map(|config| &config.query)
    });

    // The last capture of a node decides how it is highlighted, so a later capture
    // such as `@keyword` has to be able to overrule an earlier `@variable`.
    let mut colored_by_node = HashMap::<Range<usize>, bool>::default();
    while let Some(capture) = captures.peek() {
        let node_range = capture.node.byte_range();
        // Only named leaf nodes are single names. A capture of a larger node, such as a
        // whole type annotation, covers punctuation and several names, and anonymous
        // nodes are punctuation and keywords, such as the `!` of a Rust macro call.
        if capture.node.child_count() == 0
            && capture.node.is_named()
            && !node_range.is_empty()
            && range.contains(&node_range.start)
        {
            let capture_name = captures
                .grammars()
                .get(capture.grammar_index)
                .and_then(|grammar| grammar.highlights_config.as_ref())
                .and_then(|config| config.query.capture_names().get(capture.index as usize))
                // Captures that start with an underscore only feed query predicates.
                .filter(|name| !name.starts_with('_'));
            if let Some(capture_name) = capture_name {
                colored_by_node
                    .insert(node_range, is_colored_capture(capture_name, color_builtins));
            }
        }
        captures.advance();
    }

    let mut ranges = colored_by_node
        .into_iter()
        .filter_map(|(node_range, colored)| colored.then_some(node_range))
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range.start);
    ranges
}

fn is_colored_capture(capture_name: &str, color_builtins: bool) -> bool {
    if capture_name.starts_with(COMPONENT_TAG_CAPTURE) {
        return true;
    }
    let mut segments = capture_name.split('.');
    let kind = segments.next().unwrap_or_default();
    if !COLORED_CAPTURE_KINDS.contains(&kind) {
        return false;
    }
    // Grammars use `@variable.special` for names the language defines, such as `self` and `this`.
    let is_builtin = segments
        .any(|segment| segment == "builtin" || (kind == "variable" && segment == "special"));
    color_builtins || !is_builtin
}

/// Whether the text of a captured node reads as a single name.
fn is_name<'a>(text: impl Iterator<Item = &'a str>) -> bool {
    let mut has_letter = false;
    for (index, character) in text.flat_map(str::chars).enumerate() {
        // A leading quote is the start of a quoted key, while a later one can be the
        // prime of a name such as `x'`.
        if character.is_whitespace()
            || NON_NAME_CHARACTERS.contains(&character)
            || (index == 0 && character == '\'')
        {
            return false;
        }
        has_letter |= character.is_alphabetic() || character == '_';
    }
    has_letter
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use fs::FakeFs;
    use futures::StreamExt as _;
    use gpui::{Hsla, TestAppContext, UpdateGlobal as _};
    use indoc::indoc;
    use language::Capability;
    use languages::rust_lang;
    use multi_buffer::{MultiBuffer, PathKey};
    use project::Project;
    use serde_json::json;
    use settings::{
        ColorizeIdentifiersSettingsContent, GlobalLspSettingsContent, LanguageSettingsContent,
        SemanticTokenRule, SemanticTokenRules, SemanticTokens, SettingsStore,
    };
    use util::path;

    use super::*;
    use crate::{
        editor_tests::{init_test, update_test_language_settings},
        test::editor_lsp_test_context::EditorLspTestContext,
    };

    fn gradient() -> Vec<Rgba> {
        vec![gpui::rgb(0xff0000), gpui::rgb(0x0000ff)]
    }

    fn enable_identifier_colors(
        cx: &mut TestAppContext,
        color_builtins: bool,
        semantic_tokens: SemanticTokens,
    ) {
        init_test(cx, |_| {});
        update_test_language_settings(cx, &|language_settings| {
            language_settings.defaults.colorize_identifiers =
                Some(ColorizeIdentifiersSettingsContent {
                    enabled: Some(true),
                    colors: Some(gradient()),
                    color_builtins: Some(color_builtins),
                });
            language_settings.defaults.semantic_tokens = Some(semantic_tokens);
        });
    }

    fn identifier_colors(cx: &mut EditorLspTestContext) -> Vec<(String, Hsla)> {
        // Semantic tokens are requested after a debounce, which the clock has to pass.
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        cx.update_editor(|editor, window, cx| {
            snapshot_identifier_colors(&editor.snapshot(window, cx))
        })
    }

    fn snapshot_identifier_colors(snapshot: &crate::EditorSnapshot) -> Vec<(String, Hsla)> {
        let mut highlights = snapshot
            .all_text_highlight_ranges(&|key| matches!(key, HighlightKey::ColorizeIdentifier(_)));
        highlights.sort_by_key(|(_, range)| range.start);
        highlights
            .into_iter()
            .map(|(color, range)| {
                (
                    snapshot
                        .buffer_snapshot()
                        .text_for_range(range)
                        .collect::<String>(),
                    color,
                )
            })
            .collect()
    }

    fn names(colors: &[(String, Hsla)]) -> Vec<&str> {
        colors.iter().map(|(name, _)| name.as_str()).collect()
    }

    fn expected_color(name: &str) -> Hsla {
        color_at_position(&gradient(), name_color_position([name].into_iter()))
            .map(Hsla::from)
            .expect("the gradient is not empty")
    }

    async fn rust_context(cx: &mut TestAppContext) -> EditorLspTestContext {
        EditorLspTestContext::new(
            Arc::into_inner(rust_lang()).expect("a single reference to the language"),
            lsp::ServerCapabilities::default(),
            cx,
        )
        .await
    }

    #[gpui::test]
    async fn test_identifiers_are_colored_by_name(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let mut cx = rust_context(cx).await;

        cx.set_state(indoc! {r#"ˇuse crate::helper;

            fn main() {
                let total = helper(1);
                self.report(total);
            }
        "#});

        let colors = identifier_colors(&mut cx);
        assert_eq!(
            names(&colors),
            ["helper", "main", "total", "helper", "report", "total"],
            "keywords, numbers and `self` should not be colored"
        );
        for (name, color) in &colors {
            assert_eq!(
                *color,
                expected_color(name),
                "`{name}` should get the color its name hashes to, wherever it appears"
            );
        }
        assert_ne!(expected_color("helper"), expected_color("total"));
    }

    #[gpui::test]
    async fn test_imported_names_match_their_uses(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let mut cx = EditorLspTestContext::new(
            Arc::into_inner(languages::language(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            ))
            .expect("a single reference to the language"),
            lsp::ServerCapabilities::default(),
            cx,
        )
        .await;

        cx.set_state(indoc! {r#"ˇimport { createRouter } from './router.js';
            import type { BillingService } from './billing.js';

            const billing: BillingService = this.billing;
            items.forEach(createRouter);
            createRouter(billing);
        "#});

        let colors = identifier_colors(&mut cx);
        assert_eq!(
            names(&colors),
            [
                "createRouter",
                "BillingService",
                "billing",
                "BillingService",
                "billing",
                "items",
                "forEach",
                "createRouter",
                "createRouter",
                "billing",
            ],
            "keywords, strings and `this` should not be colored"
        );
        for (name, color) in &colors {
            assert_eq!(
                *color,
                expected_color(name),
                "`{name}` should have one color in imports, calls and arguments"
            );
        }
    }

    #[gpui::test]
    async fn test_component_and_attribute_names_match_their_uses(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let mut cx = EditorLspTestContext::new(
            Arc::into_inner(languages::language(
                "tsx",
                tree_sitter_typescript::LANGUAGE_TSX.into(),
            ))
            .expect("a single reference to the language"),
            lsp::ServerCapabilities::default(),
            cx,
        )
        .await;

        cx.set_state(indoc! {r#"ˇimport { Button } from './button.js';

            function App({ label }) {
                return <Button label={label}><div /></Button>;
            }
        "#});

        let colors = identifier_colors(&mut cx);
        let count = |name: &str| {
            names(&colors)
                .iter()
                .filter(|other| **other == name)
                .count()
        };
        assert_eq!(
            count("Button"),
            3,
            "import, opening and closing tag: {colors:?}"
        );
        assert_eq!(
            count("label"),
            3,
            "parameter, attribute and value: {colors:?}"
        );
        assert_eq!(
            count("div"),
            0,
            "fixed tag names are not identifiers: {colors:?}"
        );
        for (name, color) in &colors {
            assert_eq!(*color, expected_color(name));
        }
    }

    #[gpui::test]
    async fn test_text_that_is_not_a_name_is_not_colored(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let mut cx = rust_context(cx).await;

        cx.set_state("ˇfn main() { vec![total]; }");
        assert_eq!(
            names(&identifier_colors(&mut cx)),
            ["main", "vec", "total"],
            "the `!` of a macro call is captured like the macro name, but is not a name"
        );

        assert!(is_name(["total"].into_iter()));
        assert!(is_name(["to", "tal_2"].into_iter()));
        assert!(is_name(["x'"].into_iter()));
        assert!(is_name(["$element"].into_iter()));
        for text in [
            "!",
            "%",
            "1234",
            "'key'",
            "\"key\"",
            "./build.sh",
            "Array<string>|null",
            "a b",
        ] {
            assert!(!is_name([text].into_iter()), "{text:?} is not a name");
        }
    }

    #[gpui::test]
    async fn test_oversized_chunks_are_not_colored(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let mut cx = rust_context(cx).await;

        let statement = "let total = 1; ";
        let statements = MAX_BYTES_PER_CHUNK / statement.len() + 1;
        cx.set_state(&format!(
            "ˇfn main() {{ {} }}",
            statement.repeat(statements)
        ));
        assert_eq!(identifier_colors(&mut cx), Vec::new());
    }

    #[test]
    fn test_colored_captures() {
        for (capture_name, color_builtins, colored) in [
            ("variable", false, true),
            ("variable.parameter", false, true),
            ("function.method", false, true),
            ("type.class", false, true),
            ("attribute.jsx", false, true),
            ("namespace", false, true),
            ("tag.component.jsx", false, true),
            ("tag.jsx", false, false),
            ("keyword", false, false),
            ("string", false, false),
            ("punctuation.special", false, false),
            ("variable.special", false, false),
            ("variable.special", true, true),
            ("type.builtin", false, false),
            ("type.builtin", true, true),
            ("function.special", false, true),
        ] {
            assert_eq!(
                is_colored_capture(capture_name, color_builtins),
                colored,
                "@{capture_name} with color_builtins = {color_builtins}"
            );
        }
    }

    #[gpui::test]
    async fn test_builtin_names_are_colored_when_enabled(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, true, SemanticTokens::Off);
        let mut cx = rust_context(cx).await;

        cx.set_state("ˇfn main() { self.report(); }");

        let colors = identifier_colors(&mut cx);
        assert_eq!(names(&colors), ["main", "self", "report"]);
    }

    #[gpui::test]
    async fn test_identifier_colors_follow_edits_and_settings(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let mut cx = rust_context(cx).await;

        cx.set_state("ˇfn main() { total; }");
        assert_eq!(names(&identifier_colors(&mut cx)), ["main", "total"]);

        cx.set_state("ˇfn main() { count; }");
        let colors = identifier_colors(&mut cx);
        assert_eq!(names(&colors), ["main", "count"]);
        assert_eq!(colors[1].1, expected_color("count"));

        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.all_languages.defaults.colorize_identifiers =
                        Some(ColorizeIdentifiersSettingsContent {
                            enabled: Some(false),
                            ..ColorizeIdentifiersSettingsContent::default()
                        });
                });
            });
        });
        assert_eq!(
            identifier_colors(&mut cx),
            Vec::new(),
            "identifier colors should be cleared when the setting is turned off"
        );

        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.all_languages.languages.0.insert(
                        "Rust".into(),
                        LanguageSettingsContent {
                            colorize_identifiers: Some(ColorizeIdentifiersSettingsContent {
                                enabled: Some(true),
                                ..ColorizeIdentifiersSettingsContent::default()
                            }),
                            ..LanguageSettingsContent::default()
                        },
                    );
                });
            });
        });
        assert_eq!(
            names(&identifier_colors(&mut cx)),
            ["main", "count"],
            "a language can turn identifier colors on for itself"
        );
    }

    #[gpui::test]
    async fn test_identifier_colors_default_to_theme_accents(cx: &mut TestAppContext) {
        init_test(cx, |language_settings| {
            language_settings.defaults.colorize_identifiers =
                Some(ColorizeIdentifiersSettingsContent {
                    enabled: Some(true),
                    ..ColorizeIdentifiersSettingsContent::default()
                });
        });
        let mut cx = rust_context(cx).await;

        cx.set_state("ˇfn main() { total; total; }");

        let colors = identifier_colors(&mut cx);
        assert_eq!(names(&colors), ["main", "total", "total"]);
        let theme_accents = cx.update_editor(|editor, _, _| editor.theme_accent_colors());
        assert!(!theme_accents.is_empty());
        for (name, color) in &colors {
            assert_eq!(
                Some(*color),
                color_at_position(
                    &theme_accents,
                    name_color_position([name.as_str()].into_iter())
                )
                .map(Hsla::from),
                "`{name}` should get its color from the accent colors of the theme"
            );
        }
    }

    #[gpui::test]
    async fn test_identifiers_are_colored_as_they_scroll_into_view(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let mut cx = rust_context(cx).await;

        let rows_between = 3 * ROWS_PER_CHUNK as usize;
        cx.set_state(&format!(
            "ˇfn first() {{}}\n{}fn last() {{}}\n",
            "//\n".repeat(rows_between)
        ));
        assert_eq!(
            names(&identifier_colors(&mut cx)),
            ["first"],
            "only the chunk with visible rows should be colorized"
        );

        cx.update_editor(|editor, window, cx| {
            editor.move_to_end(&crate::MoveToEnd, window, cx);
        });
        assert_eq!(
            names(&identifier_colors(&mut cx)),
            ["first", "last"],
            "scrolling should add the new chunk and keep the earlier one"
        );
    }

    #[gpui::test]
    async fn test_identifiers_are_colored_in_multi_buffer_excerpts(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, false, SemanticTokens::Off);
        let fs = FakeFs::new(cx.background_executor.clone());
        fs.insert_tree(
            path!("/a"),
            json!({
                "main.rs": "fn main() { total; }\n\n\nfn hidden() {}\n",
                "lib.rs": "fn outside() {}\nfn library() { total; }\n",
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/a").as_ref()], cx).await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(rust_lang());
        let main_buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/a/main.rs"), cx)
            })
            .await
            .expect("main.rs should open");
        let library_buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer(path!("/a/lib.rs"), cx)
            })
            .await
            .expect("lib.rs should open");

        let multi_buffer = cx.new(|cx| {
            let mut multi_buffer = MultiBuffer::new(Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(0),
                main_buffer.clone(),
                [Point::new(0, 0)..Point::new(1, 0)],
                0,
                cx,
            );
            multi_buffer.set_excerpts_for_path(
                PathKey::sorted(1),
                library_buffer.clone(),
                [Point::new(1, 0)..Point::new(2, 0)],
                0,
                cx,
            );
            multi_buffer
        });
        let editor = cx.add_window(|window, cx| {
            Editor::for_multibuffer(multi_buffer, Some(project.clone()), window, cx)
        });
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.executor().run_until_parked();

        let colors = editor
            .update(cx, |editor, window, cx| {
                snapshot_identifier_colors(&editor.snapshot(window, cx))
            })
            .expect("the editor window should be open");
        assert_eq!(
            names(&colors),
            ["main", "total", "library", "total"],
            "only identifiers inside the excerpts should be colored"
        );
        assert_eq!(colors[1].1, expected_color("total"));
        assert_eq!(colors[3].1, expected_color("total"));
    }

    #[gpui::test]
    async fn test_identifier_colors_win_over_semantic_token_colors(cx: &mut TestAppContext) {
        enable_identifier_colors(cx, true, SemanticTokens::Combined);
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.global_lsp_settings = Some(GlobalLspSettingsContent {
                        semantic_token_rules: Some(SemanticTokenRules {
                            rules: vec![SemanticTokenRule {
                                token_type: Some("function".to_string()),
                                foreground_color: Some(gpui::rgb(0x00ff00)),
                                ..SemanticTokenRule::default()
                            }],
                        }),
                        ..GlobalLspSettingsContent::default()
                    });
                });
            });
        });
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                semantic_tokens_provider: Some(
                    lsp::SemanticTokensServerCapabilities::SemanticTokensOptions(
                        lsp::SemanticTokensOptions {
                            legend: lsp::SemanticTokensLegend {
                                token_types: vec!["function".into()],
                                token_modifiers: Vec::new(),
                            },
                            full: Some(lsp::SemanticTokensFullOptions::Bool(true)),
                            ..lsp::SemanticTokensOptions::default()
                        },
                    ),
                ),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;
        let mut full_request = cx
            .set_request_handler::<lsp::request::SemanticTokensFullRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(lsp::SemanticTokensResult::Tokens(
                        lsp::SemanticTokens {
                            data: vec![
                                0, 3, 4, 0, 0, // main: function
                            ],
                            result_id: None,
                        },
                    )))
                },
            );

        cx.set_state("ˇfn main() {}");
        full_request.next().await;
        assert_eq!(names(&identifier_colors(&mut cx)), ["main"]);

        let painted_colors = cx.update_editor(|editor, window, cx| {
            editor
                .snapshot(window, cx)
                .display_snapshot
                .chunks(
                    crate::display_map::DisplayRow(0)..crate::display_map::DisplayRow(1),
                    language::LanguageAwareStyling {
                        tree_sitter: false,
                        diagnostics: false,
                    },
                    crate::HighlightStyles::default(),
                )
                .filter(|chunk| chunk.text == "main")
                .map(|chunk| chunk.highlight_style.and_then(|style| style.color))
                .collect::<Vec<_>>()
        });
        assert_eq!(painted_colors, [Some(expected_color("main"))]);
    }

    #[gpui::test]
    async fn test_standard_library_tokens_are_not_colored(cx: &mut TestAppContext) {
        assert_standard_library_token_colors(false, &["main", "total"], cx).await;
    }

    #[gpui::test]
    async fn test_standard_library_tokens_are_colored_when_enabled(cx: &mut TestAppContext) {
        assert_standard_library_token_colors(true, &["main", "println", "total"], cx).await;
    }

    async fn assert_standard_library_token_colors(
        color_builtins: bool,
        expected_names: &[&str],
        cx: &mut TestAppContext,
    ) {
        enable_identifier_colors(cx, color_builtins, SemanticTokens::Combined);
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                semantic_tokens_provider: Some(
                    lsp::SemanticTokensServerCapabilities::SemanticTokensOptions(
                        lsp::SemanticTokensOptions {
                            legend: lsp::SemanticTokensLegend {
                                token_types: vec!["function".into()],
                                token_modifiers: vec![lsp::SemanticTokenModifier::DEFAULT_LIBRARY],
                            },
                            full: Some(lsp::SemanticTokensFullOptions::Bool(true)),
                            ..lsp::SemanticTokensOptions::default()
                        },
                    ),
                ),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;
        let mut full_request = cx
            .set_request_handler::<lsp::request::SemanticTokensFullRequest, _, _>(
                move |_, _, _| async move {
                    Ok(Some(lsp::SemanticTokensResult::Tokens(
                        lsp::SemanticTokens {
                            data: vec![
                                0, 12, 7, 0, 1, // println: function, defaultLibrary
                            ],
                            result_id: None,
                        },
                    )))
                },
            );

        cx.set_state("ˇfn main() { println(total); }");
        full_request.next().await;

        assert_eq!(
            names(&identifier_colors(&mut cx)),
            expected_names,
            "color_builtins: {color_builtins}"
        );
    }
}
