# Changelog

What changed in CST, newest first. Written for people using it, not for the code.
Earlier releases: https://github.com/tupini07/copilot-session-tui/releases

## v0.33.1 - 2026-10-02

- Ctrl+click a link in a session to open it, including links that show text instead of an address.
- Web links open on your own computer, even when CST runs on another machine over SSH.
- Clicking a link to a file shows it in its folder instead of running it.

## v0.33.0 - 2026-09-24

- Note: a turn that finishes while you are watching its pane no longer sends a notification.
- A tab no longer keeps spinning after its turn ended while a background shell is still running.
- A stray backspace with nothing typed no longer marks a tab as having an unsent draft.
- A thread notice now survives restarting CST instead of being lost with the comment that caused it.

## v0.32.1 - 2026-09-16

- Thread notices arrive one at a time, instead of several stacked into a single message.

## v0.32.0 - 2026-09-15

- Ask Copilot for a review, and see which review threads are resolved, in the GitHub inspector.
- Filter a pull request's comments to the unresolved ones, and pick which filter you start on.
- A notification no longer lands in the middle of a message you are typing.
- Several replies to one thread no longer arrive stacked into a single message.
- Name a colleague under Trusted Authors so their agent's comments can wake your sessions.

## v0.31.2 - 2026-09-15

- An agent can now post to a thread instead of reporting the command as unavailable.

## v0.31.1 - 2026-09-15

- An agent woken on a thread now sees the whole conversation, discussions included.
- Agents sharing a thread now check what has already been said before answering.

## v0.31.0 - 2026-09-15

- Take part in a GitHub issue, pull request or discussion, and be woken when somebody replies.
- Messages CST will not act on by itself wait under Waiting for you, with how long they have waited.
- A comment from anybody but you never starts a session on its own.
- CST tells you when two agents start going in circles on a thread.
- Turn thread wake-ups off for one repository, whatever your global setting says.
- A session that has finished no longer shows the progress spinner for something it left running.

## v0.30.0 - 2026-09-14

- Choose how far autopilot may carry on by itself, under Max Autopilot Continues in Global Settings.
- Give one repository its own autopilot limit, for a codebase that should stay on a shorter leash.

## v0.29.0 - 2026-09-11

- Run cst doctor to see whether Copilot CLI, gh and Git are set up, and what each one you are missing would give you.
- Hide sessions you never want to see by title or folder, and press H to bring them back temporarily.
- Move a session tab: drag it along the strip, or press prefix m and use the arrows.
- Drag the scrollbars in the issue and pull request viewer instead of only scrolling.
- Edit snippets with the same editor as the scratchpad, arrow keys and all.
- Decide per repository whether its sessions start in yolo mode, whatever your global setting says.
- After updating, CST now shows what changed since the version you were last on.
- Session exclusion filters contributed by Jake Smith.

## v0.28.0 - 2026-09-04

- Tell an unread tab apart from one waiting on your answer at a glance.
- The chat cursor no longer flickers in Alacritty.

## v0.27.0 - 2026-09-04

- Attached sessions get a real tab bar, so switching between them is one keystroke.
- Favorites can open as panes inside CST instead of as separate terminal tabs.
- Ctrl+J no longer submits the prompt by accident.
- A turn's progress no longer gets lost when Copilot reports its session late.
- Lifecycle hooks are given enough time to run on a loaded machine.

## v0.26.0 - 2026-09-01

- Command dialogs only offer what makes sense where you are.

## v0.25.0 - 2026-09-01

- Pane titles show the session name without Copilot's branding taking up the room.
- Copilot's colours follow the CST theme you picked.
- Lifecycle hooks are refreshed automatically after CST updates itself.

## v0.24.0 - 2026-08-28

- Install CST with a single command on Windows or Linux.
- Press the prefix key to get a menu of what it can do, instead of memorising it.
- Update from the shell with cst update, without opening the TUI.
- Session progress is reported by Copilot itself rather than guessed from its output.
- Read a GitHub discussion in the inspector, alongside issues and pull requests.
- A merged pull request is shown as merged rather than just closed.
