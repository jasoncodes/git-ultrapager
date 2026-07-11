//! `two-face` grammarへhunk先頭state推定用の内部entryを追加するbuild-time変換。
//!
//! Runtimeは生成済み[`SyntaxSet`]とscope別marker・opener/exit regexをdeserializeする。候補は
//! grammar内のpush/set stackを複製し、backreferenceを使わず、dark/light themeのどちらかで
//! 通常stateと異なるstyleを持ち、include先を含めてpop可能なものに絞る。元の`__start`へNUL
//! marker規則を追加するが、sanitized sourceにはNULが残らないため通常入力からは到達できない。

use std::collections::{HashMap, HashSet};

use syntect::highlighting::{Highlighter, ThemeSet};
use syntect::parsing::syntax_definition::{
    Context, ContextId, ContextReference, MatchOperation, MatchPattern, Pattern,
};
use syntect::parsing::{Scope, SyntaxDefinition, SyntaxSet, SyntaxSetBuilder};

const MAX_CONTINUATION_CANDIDATES: usize = 64;

type SerializedContinuationCandidate = (String, Vec<String>, Vec<String>);
type SerializedContinuationCandidates = HashMap<Scope, Vec<SerializedContinuationCandidate>>;
pub type GeneratedSyntaxAsset = (SyntaxSet, SerializedContinuationCandidates);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContinuationStack {
    contexts: Vec<ContextReference>,
    with_prototype: Option<ContextReference>,
}

struct ContinuationTransition {
    stack: ContinuationStack,
    opener_pattern: String,
}

/// Runtimeがそのままdeserializeできるsyntax setとscope別(marker, opener regex, pop regex)を生成する。
pub fn build() -> GeneratedSyntaxAsset {
    let themes = ThemeSet::load_defaults();
    let base = two_face::syntax::extra_newlines();
    let builder = base.clone().into_builder();
    let mut definitions = builder.syntaxes().to_vec();
    let context_references = definitions
        .iter()
        .flat_map(pushed_context_transitions)
        .flat_map(|transition| transition.stack.contexts)
        .collect::<Vec<_>>();
    let context_reference_by_id = context_references
        .iter()
        .filter_map(|reference| reference.id().ok().map(|id| (id, reference.clone())))
        .collect::<HashMap<_, _>>();
    let mut candidates = HashMap::new();

    for definition in &mut definitions {
        let markers = add_continuation_markers(
            definition,
            &base,
            &context_references,
            &context_reference_by_id,
            &themes,
        );
        if !markers.is_empty() {
            candidates.insert(definition.scope, markers);
        }
    }

    let mut rebuilt = SyntaxSetBuilder::new();
    for definition in definitions {
        rebuilt.add(definition);
    }
    (rebuilt.build(), candidates)
}

fn add_continuation_markers(
    definition: &mut SyntaxDefinition,
    syntax_set: &SyntaxSet,
    context_references: &[ContextReference],
    context_reference_by_id: &HashMap<ContextId, ContextReference>,
    themes: &ThemeSet,
) -> Vec<SerializedContinuationCandidate> {
    let Some(main) = definition.contexts.get("__main") else {
        return Vec::new();
    };
    let Some(main_reference) = reference_to_context(syntax_set, context_references, main) else {
        return Vec::new();
    };

    let mut transitions = pushed_context_transitions(definition)
        .into_iter()
        .filter_map(|transition| {
            let contexts = transition
                .stack
                .contexts
                .iter()
                .map(|reference| reference.resolve(syntax_set))
                .collect::<Result<Vec<_>, _>>();
            let Ok(contexts) = contexts else {
                return None;
            };
            let eligible = !contexts.iter().any(|context| context.uses_backrefs)
                && contexts
                    .last()
                    .is_some_and(|context| context_can_pop(context, syntax_set, 0))
                && context_group_changes_visible_style(definition.scope, &contexts, themes);
            if !eligible {
                return None;
            }
            let mut pop_patterns = Vec::new();
            let mut visited = HashSet::new();
            // 列挙が途中で途切れても、得られたregexは候補を除外するprefilterとして使える。
            // 採否はruntimeのparse state比較が決めるため、不完全な列挙を採用条件にはしない。
            let _ = contexts.last().is_some_and(|context| {
                collect_pop_patterns(
                    context,
                    syntax_set,
                    context_reference_by_id,
                    0,
                    &mut visited,
                    &mut pop_patterns,
                )
            }) && transition
                .stack
                .with_prototype
                .as_ref()
                .is_none_or(|prototype| {
                    prototype.resolve(syntax_set).is_ok_and(|context| {
                        collect_pop_patterns(
                            context,
                            syntax_set,
                            context_reference_by_id,
                            0,
                            &mut visited,
                            &mut pop_patterns,
                        )
                    })
                });
            pop_patterns.sort_unstable();
            pop_patterns.dedup();
            pop_patterns.retain(|pattern| exit_pattern_is_discriminating(pattern));
            if pop_patterns.is_empty() {
                return None;
            }
            Some((transition.stack, transition.opener_pattern, pop_patterns))
        })
        .collect::<Vec<_>>();
    transitions.sort_by_cached_key(|(stack, _, _)| format!("{stack:?}"));
    let mut context_groups: Vec<(ContinuationStack, Vec<String>, Vec<String>)> = Vec::new();
    for (stack, opener_pattern, pop_patterns) in transitions {
        if let Some((current, opener_patterns, current_pop_patterns)) = context_groups.last_mut()
            && *current == stack
        {
            opener_patterns.push(opener_pattern);
            current_pop_patterns.extend(pop_patterns);
        } else {
            context_groups.push((stack, vec![opener_pattern], pop_patterns));
        }
    }
    for (_, opener_patterns, pop_patterns) in &mut context_groups {
        opener_patterns.sort_unstable();
        opener_patterns.dedup();
        pop_patterns.sort_unstable();
        pop_patterns.dedup();
    }
    context_groups.truncate(MAX_CONTINUATION_CANDIDATES);

    let mut marker_patterns = Vec::with_capacity(context_groups.len());
    let mut markers = Vec::with_capacity(context_groups.len());
    for (index, (context_group, opener_patterns, pop_patterns)) in
        context_groups.into_iter().enumerate()
    {
        let marker = format!("\0GUP-CONTINUATION-{index:02x}\0");
        let mut seeded_stack = Vec::with_capacity(1 + context_group.contexts.len());
        seeded_stack.push(main_reference.clone());
        seeded_stack.extend(context_group.contexts);
        marker_patterns.push(Pattern::Match(MatchPattern::new(
            false,
            format!(r"\A{}", regex::escape(&marker)),
            Vec::new(),
            None,
            MatchOperation::Push(seeded_stack),
            context_group.with_prototype,
        )));
        markers.push((marker, opener_patterns, pop_patterns));
    }

    if let Some(start) = definition.contexts.get_mut("__start") {
        start.patterns.splice(0..0, marker_patterns);
    } else {
        markers.clear();
    }
    markers
}

fn exit_pattern_is_discriminating(pattern: &str) -> bool {
    const ORDINARY_LINES: &[&str] = &["\n", "identifier\n", " value\n", ")\n", "}\n", ";\n"];
    let pattern = syntect::parsing::Regex::new(pattern.to_owned());
    !ORDINARY_LINES
        .iter()
        .any(|line| pattern.search(line, 0, line.len(), None))
}

fn context_group_changes_visible_style(
    syntax_scope: Scope,
    contexts: &[&Context],
    themes: &ThemeSet,
) -> bool {
    let scope_count = contexts
        .iter()
        .map(|context| context.meta_scope.len() + context.meta_content_scope.len())
        .sum::<usize>();
    let mut continuation_stack = Vec::with_capacity(1 + scope_count);
    continuation_stack.push(syntax_scope);
    for context in contexts {
        continuation_stack.extend_from_slice(&context.meta_scope);
        continuation_stack.extend_from_slice(&context.meta_content_scope);
    }

    ["base16-ocean.dark", "InspiredGitHub"]
        .into_iter()
        .filter_map(|name| themes.themes.get(name))
        .any(|theme| {
            let highlighter = Highlighter::new(theme);
            let baseline = highlighter.style_for_stack(&[syntax_scope]);
            let continuation = highlighter.style_for_stack(&continuation_stack);
            baseline.foreground != continuation.foreground
                || baseline.font_style != continuation.font_style
        })
}

fn pushed_context_transitions(definition: &SyntaxDefinition) -> Vec<ContinuationTransition> {
    let mut context_names = definition.contexts.keys().collect::<Vec<_>>();
    context_names.sort_unstable();
    let mut groups = Vec::new();
    for context_name in context_names {
        let context = &definition.contexts[context_name];
        for pattern in &context.patterns {
            if let Pattern::Match(pattern) = pattern
                && let MatchOperation::Push(pushed) | MatchOperation::Set(pushed) =
                    &pattern.operation
                && !pushed.is_empty()
            {
                groups.push(ContinuationTransition {
                    stack: ContinuationStack {
                        contexts: pushed.clone(),
                        with_prototype: pattern.with_prototype.clone(),
                    },
                    opener_pattern: pattern.regex().regex_str().to_owned(),
                });
            }
        }
    }
    groups
}

fn context_can_pop(context: &Context, syntax_set: &SyntaxSet, depth: usize) -> bool {
    const MAX_INCLUDE_DEPTH: usize = 16;
    if depth == MAX_INCLUDE_DEPTH {
        return false;
    }
    context.patterns.iter().any(|pattern| match pattern {
        Pattern::Match(pattern) => matches!(pattern.operation, MatchOperation::Pop),
        Pattern::Include(reference) => reference
            .resolve(syntax_set)
            .is_ok_and(|included| context_can_pop(included, syntax_set, depth + 1)),
    })
}

fn collect_pop_patterns(
    context: &Context,
    syntax_set: &SyntaxSet,
    context_reference_by_id: &HashMap<ContextId, ContextReference>,
    depth: usize,
    visited: &mut HashSet<*const Context>,
    output: &mut Vec<String>,
) -> bool {
    const MAX_TRANSITION_DEPTH: usize = 16;
    if depth == MAX_TRANSITION_DEPTH || context.uses_backrefs {
        return false;
    }
    if !visited.insert(std::ptr::from_ref(context)) {
        return true;
    }

    let mut complete = true;
    for pattern in &context.patterns {
        match pattern {
            Pattern::Match(pattern) => match &pattern.operation {
                MatchOperation::Pop => output.push(pattern.regex().regex_str().to_owned()),
                MatchOperation::Set(contexts) => {
                    for reference in contexts {
                        complete &= reference.resolve(syntax_set).is_ok_and(|next| {
                            collect_pop_patterns(
                                next,
                                syntax_set,
                                context_reference_by_id,
                                depth + 1,
                                visited,
                                output,
                            )
                        });
                    }
                    if let Some(prototype) = pattern.with_prototype.as_ref() {
                        complete &= prototype.resolve(syntax_set).is_ok_and(|next| {
                            collect_pop_patterns(
                                next,
                                syntax_set,
                                context_reference_by_id,
                                depth + 1,
                                visited,
                                output,
                            )
                        });
                    }
                }
                MatchOperation::Push(_) | MatchOperation::None => {}
            },
            Pattern::Include(reference) => {
                complete &= reference.resolve(syntax_set).is_ok_and(|included| {
                    collect_pop_patterns(
                        included,
                        syntax_set,
                        context_reference_by_id,
                        depth + 1,
                        visited,
                        output,
                    )
                });
            }
        }
    }
    if let Some(prototype_id) = context.prototype {
        complete &= context_reference_by_id
            .get(&prototype_id)
            .and_then(|reference| reference.resolve(syntax_set).ok())
            .is_some_and(|prototype| {
                collect_pop_patterns(
                    prototype,
                    syntax_set,
                    context_reference_by_id,
                    depth + 1,
                    visited,
                    output,
                )
            });
    }
    complete
}

fn reference_to_context(
    syntax_set: &SyntaxSet,
    references: &[ContextReference],
    target: &Context,
) -> Option<ContextReference> {
    references
        .iter()
        .find(|reference| {
            reference
                .resolve(syntax_set)
                .is_ok_and(|context| context == target)
        })
        .cloned()
}
