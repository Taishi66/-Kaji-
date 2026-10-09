## Strengths

| Capability | What this means in practice |
| --- | --- |
| Tool use and verification | FIRST_TOOL_RECORD I read and write files, inspect Git changes, run builds and meaningful checks, and report the evidence rather than only producing text. COMPLETE_TOOLS |
| Parallel exploration | I combine independent sources when the work allows it, then reconcile their answers into one clear explanation with concrete supporting details. COMPLETE_PARALLEL |
| Navigating unfamiliar projects | I map the directory tree, signatures, callers and constraints before changing the implementation, including long paths and unfamiliar Unicode names such as 鍛冶 and café. COMPLETE_NAVIGATION |
| Project context | I follow the repository instructions, preserve existing work and keep local exploration distinct from changes ready for review and publication. COMPLETE_CONTEXT |

Repeated paragraph: the interface should keep the exact passage in view when the terminal changes size. This sentence appears twice so an anchor cannot silently jump to the first occurrence.

## Weaknesses

| Capability | What this means in practice |
| --- | --- |
| Tool use and verification | SECOND_TOOL_RECORD I can receive an incomplete answer or choose the wrong tool. Verification must show the final evidence, including the end of a long table cell that used to disappear behind an ellipsis. COMPLETE_SECOND_TOOLS |
| Navigating unfamiliar projects | SECOND_NAV_RECORD An incorrect assumption can look plausible in an unfamiliar codebase. I need to read the relevant sources and test the behavior under the real constraints rather than guess. COMPLETE_SECOND_NAVIGATION |
| Finite context and repeated passages | SECOND_CONTEXT_RECORD A long investigation has many repeated labels and similar paragraphs. Resizing must retain the source position of the passage being read, even when this table becomes stacked records. COMPLETE_SECOND_CONTEXT |
| External dependencies | The available provider, configured extensions and network connection affect what can run. A clear interface separates the current result from waiting activity without burying either in decoration. COMPLETE_DEPENDENCIES |

Repeated paragraph: the interface should keep the exact passage in view when the terminal changes size. This sentence appears twice so an anchor cannot silently jump to the first occurrence.

## A concrete example

A postal code was interpreted as a location in a different country. Comparing the first result with a second source exposed the mismatch; using the actual coordinates fixed the answer. The lesson is to make verification visible and keep complete explanations readable at every available width. COMPLETE_EXAMPLE

## Getting the best result

Give a concrete goal, a reference, the important constraints and an observable success condition. The transcript should make each of those easy to inspect while keeping the composer and the current status available. READING_FIXTURE_DONE
