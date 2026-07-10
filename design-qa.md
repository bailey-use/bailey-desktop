# Bailey desktop layout design QA

## Evidence

- Source visual truth: `/var/folders/rl/6pbp9_k55qn_kj294byvss8m0000gp/T/codex-clipboard-65b37d60-ad0b-477f-bd7a-b6ec685fd1b0.png`
- Normalized source: `design-qa/codex-layout-reference.png`
- Browser-rendered implementation: `design-qa/bailey-layout-implementation.png`
- Full-view combined comparison: `design-qa/codex-bailey-comparison.png`
- Focused sidebar comparison: `design-qa/codex-bailey-sidebar-comparison.png`
- Focused composer comparison: `design-qa/codex-bailey-composer-comparison.png`
- Minimum supported window capture: `design-qa/bailey-layout-min-window.png`
- Primary viewport: `1440 × 817`, dark theme, active project and active task, transcript with user/tool/assistant items, idle composer.
- Responsive viewport: `900 × 600`, matching the Tauri window minimum width.

## Findings

No actionable P0, P1, or P2 differences remain for the requested scope: learn the Codex layout relationships without copying its branding, content, or unavailable features.

- Fonts and typography: both views use a compact system sans-serif hierarchy. Bailey keeps its existing heavier wordmark and lime brand accent intentionally; task, project, transcript, and control sizes preserve the source hierarchy and do not clip or wrap incorrectly.
- Spacing and layout rhythm: the final desktop frame uses the measured `520 / 920` sidebar-to-main split at the reference viewport. Transcript and composer share a centered `736px` column. The composer surface is `100px` high, project/task rows are compact, and the `900 × 600` check has no overlap or hidden persistent controls.
- Colors and tokens: the source's dark neutral structure is retained while Bailey's olive/lime palette remains product-specific. This is an intentional brand substitution, not layout drift. Contrast and visible focus rings remain present.
- Image and asset fidelity: the target has no required photographic or illustrative assets. Visible interface icons use the Phosphor icon library; no emoji, handcrafted SVG, CSS drawing, or placeholder imagery substitutes were introduced.
- Copy and content: Codex product labels and unsupported Scheduled/Plugins/Sites/Chat entries were deliberately not copied. Bailey copy describes real local execution, projects, tasks, model selection, approvals, and runtime state.
- States and interactions: project search, project opening in preview, model popover, message submission, tool disclosure rows, approval/user-input controls, and stop behavior were exercised. Browser console error check returned zero errors.
- Native packaging: the final Tauri macOS app bundle built and launched with its sidecar. The browser comparison excludes native macOS window chrome; no extra fake titlebar was added to the web layout.

## Comparison history

### Pass 1 — blocked

The initial same-input comparison found three layout-density issues:

- Main transcript/composer column was about `24px` wider than the source.
- Composer surface was visibly taller than the source.
- Sidebar project/task rows and active-task treatment were too loose and card-like.

Fixes applied:

- Reduced the shared content column from `760px` to `736px`.
- Reduced composer surface height to `100px`, tightened textarea/toolbar padding, and fixed the textarea height.
- Reduced New task, project, and task row heights and lowered active-task background contrast.

### Pass 2 — passed

Post-fix evidence is recorded in `design-qa/codex-bailey-comparison.png`, with readable focused checks in `design-qa/codex-bailey-sidebar-comparison.png` and `design-qa/codex-bailey-composer-comparison.png`. The sidebar split, centered transcript axis, composer width/height, nested Project → Task hierarchy, and bottom anchoring now match the reference layout. Remaining visible differences are expected Bailey branding, dynamic transcript content, omitted unavailable Codex destinations, and native window chrome.

## Primary interactions tested

- Toggle project search.
- Open/reset a project in layout-preview mode without invoking unavailable browser-side Tauri APIs.
- Open and cancel the model picker.
- Submit a follow-up message and render the resulting user/activity/assistant sequence.
- Render compact and minimum-window states at `1440 × 817` and `900 × 600`.
- Check browser error logs after navigation and interaction: no errors.

## Follow-up polish

- P3: a future native-titlebar pass may adopt an overlay titlebar after the window behavior is intentionally designed; it is not required for the requested content layout.

final result: passed
