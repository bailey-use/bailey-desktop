# Bailey project conversation flow audit

Audit date: 2026-07-10

Viewport: 1280 × 720, dark theme, `layoutPreview=1`

Overall health: **Healthy after fixes.** The project-level conversation loop is usable by mouse and keyboard, preserves prior tasks, and returns focus predictably. No browser console errors were recorded.

## Flow

1. Open an existing project and task.
   - Health: healthy.
   - The selected project contains a visible nested task, and the main panel restores its transcript.
   - Evidence: [01-existing-task.png](01-existing-task.png)

2. Create a new task inside the current project.
   - Before: blocked. The new task replaced the only in-memory conversation, so the existing task disappeared.
   - After: healthy. The new task is added as a sibling under the same project, prior tasks remain visible, and the empty composer receives focus.
   - Before evidence: [02-new-task-replaces-current.png](02-new-task-replaces-current.png)
   - After evidence: [05-new-task-preserves-history-and-focus.png](05-new-task-preserves-history-and-focus.png)

3. Enter and send the first prompt.
   - Before: degraded. Mouse submission left `document.activeElement` on `BODY`, forcing another click before continuing.
   - After: healthy. The task title updates from the first prompt, the message is appended to that task, and focus returns to the composer after the send acknowledgement.
   - Before evidence: [03-sent-task-loses-focus.png](03-sent-task-loses-focus.png)
   - Runtime assertion after fix: active element was `TEXTAREA[aria-label="任务内容"]`.

4. Switch back to a previous task.
   - Health: healthy.
   - The previous transcript is restored, the newly created sibling remains in the sidebar, and composer focus is restored.
   - Evidence: [06-switched-task-restores-transcript.png](06-switched-task-restores-transcript.png)

5. Open and close the model picker.
   - Before: degraded. The popover opened while focus stayed on its trigger, and Escape did not close the picker from the intended input workflow.
   - After: healthy. The model input receives focus on open; Escape closes the dialog and returns focus to the model trigger.
   - Before evidence: [04-model-popover-keeps-trigger-focus.png](04-model-popover-keeps-trigger-focus.png)
   - After evidence: [07-model-popover-focus-fixed.png](07-model-popover-focus-fixed.png)

6. Use search and task-menu keyboard dismissal.
   - Health: healthy.
   - Search autofocuses its labeled input; Escape closes it and returns focus to the search trigger. The task disclosure also returns focus to its trigger on Escape.

7. Receive an approval or input request.
   - Health: implementation-reviewed; native live-request capture remains a test gap.
   - The interaction is an `aria-modal` dialog, background regions become inert, focus moves into the dialog, Tab/Shift+Tab are trapped, and Escape is IME-safe.

8. Recover from a conversation save failure.
   - Health: healthy, implementation and protocol tested.
   - A failed terminal persist marks the task as unsaved, blocks task/project switching, exposes a non-agent `thread/flush` retry in the runtime-status panel, and intercepts window close until the user saves or explicitly confirms discarding the in-memory turn.

9. Reopen the same task from another Bailey process.
   - Health: healthy for App Server clients.
   - A kernel-backed session lease rejects concurrent resume/delete, releases automatically after normal exit or SIGKILL, protects loaded sessions from retention eviction, and prevents last-writer-wins transcript corruption.

## Fixed findings

- **P0 — New task destroyed the visible conversation list.** Replaced the single global conversation state with project-scoped conversation records and explicit active-session selection.
- **P1 — Composer focus was not restored.** Added cancellable focus scheduling for new, switch, send, stop, and interaction completion paths without re-focusing on every streamed transcript item.
- **P1 — A corrupt newest session hid valid older sessions.** Project opening now attempts listed sessions in recency order and falls back to the next recoverable task.
- **P1 — A failed durable save could be lost on task switch.** Conversations are marked durability-dirty when a terminal event reports `persisted: false`; project/task switching is blocked until a later successful turn persists the transcript.
- **P1 — Closing Bailey could bypass the dirty guard.** Added browser unload protection plus Tauri close-request confirmation, and added `thread/flush` so saving can be retried without executing another Agent turn.
- **P1 — Two app processes could write or delete one session.** Added OS session leases shared by resume, delete, retention, and key-removal cleanup.
- **P1 — Cancelling after tool execution lost exact resume context.** Cancellation now exports and persists the interrupted AgentEngine transcript before emitting its terminal event; interrupted tool tails are repaired on resume.
- **P1 — Retention could evict a loaded task.** Capacity cleanup skips leased sessions and uses index-first, two-phase deletion with deterministic failure tests.
- **P2 — Overlay keyboard behavior was inconsistent.** Added focus return, outside-click handling, IME guards, and accurate disclosure/dialog semantics.
- **P2 — Failed send could lose user work.** The draft is cleared only after `turn/start` succeeds.
- **P2 — Unrecoverable newest history hid the whole project.** Project open falls back through older sessions; when none can resume, Bailey keeps their summaries visible and creates a usable new task with a warning.

## Accessibility and evidence limits

- Stable accessible labels are present for the main composer, search input, model dialog, task menu, and runtime status controls.
- Focus indicators remain visible in the Bailey accent color.
- Chinese IME composition is guarded for the composer, model input, interaction free text, and Escape handlers.
- Browser error log after the exercised flows: zero errors.
- Layout-preview mode exercises the complete frontend state machine without a native sidecar. Durable thread, cross-process lease, cancellation, eviction, delete, and flush behavior are separately covered by Rust app-server integration/unit tests; a real approval request was not screenshotted in this browser audit.

## Intentional boundary

Bailey currently serializes task execution: while a turn is running, creating or switching tasks is disabled. This avoids routing approvals or user-input requests to the wrong conversation. Parallel background turns are not claimed in this release.
