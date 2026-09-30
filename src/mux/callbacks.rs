use crate::mux::MuxEvent;
use std::sync::mpsc::Sender;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PaneSignals {
    pub title: Option<String>,
    pub events: Vec<PaneSignalEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneSignalEvent {
    Bell,
    Progress(crate::host_terminal::ProgressState),
}

/// Replies the emulator owes the child process.
///
/// `vt100` is a screen model, not a full terminal: it never answers device queries.
/// That is not optional in practice — ConPTY emits `ESC[6n` (report cursor position)
/// while starting up and *blocks* until it gets a response, so without this the very
/// first child produces no output at all.
pub struct PaneCallbacks {
    pane_id: crate::mux::PaneId,
    replies: Sender<Vec<u8>>,
    events: Sender<MuxEvent>,
    title: Option<String>,
    signals: Vec<PaneSignalEvent>,
    terminal_light_mode: Option<bool>,
    theme_updates_requested: bool,
    /// A hyperlink the child has started and not yet closed.
    open_link: Option<OpenLink>,
    /// Hyperlinks the child has drawn, newest last.
    ///
    /// Kept here because `vt100` has no notion of a hyperlink: its cells hold text and
    /// style only, and OSC 8 falls through to this callback. Without this a link reached
    /// the pane as underlined text and nothing else, so the outer terminal had nothing to
    /// open — only a URL spelled out in full could be clicked, and only because Windows
    /// Terminal recognises those by sight.
    links: std::collections::VecDeque<LinkSpan>,
}

/// How many links are remembered at once.
///
/// Comfortably more than fit on a screen. The child redraws the same links over and over
/// and each redraw replaces its earlier copy, so this only bounds the leftovers of links
/// that have scrolled away or been drawn over.
const MAX_LINKS: usize = 256;

struct OpenLink {
    start: (u16, u16),
    alternate: bool,
    target: String,
}

/// A link that is still on screen, for whoever needs to draw or reach it.
pub struct LiveLink {
    /// First cell, as (row, column).
    pub start: (u16, u16),
    /// One past the last cell, as (row, column).
    pub end: (u16, u16),
    pub target: String,
}

/// One hyperlink as drawn: where it sat, what it read, and where it pointed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LinkSpan {
    /// First cell, as (row, column).
    start: (u16, u16),
    /// One past the last cell, as (row, column).
    end: (u16, u16),
    alternate: bool,
    /// The text those cells held when the link was drawn.
    ///
    /// Checked again at click time. The child redraws freely, and a click must never
    /// open a link that has since been painted over with something else.
    text: String,
    target: String,
}

impl PaneCallbacks {
    pub fn new(
        pane_id: crate::mux::PaneId,
        replies: Sender<Vec<u8>>,
        events: Sender<MuxEvent>,
        terminal_light_mode: Option<bool>,
    ) -> Self {
        Self {
            pane_id,
            replies,
            events,
            title: None,
            signals: Vec::new(),
            terminal_light_mode,
            theme_updates_requested: false,
            open_link: None,
            links: std::collections::VecDeque::new(),
        }
    }

    /// Where the link under a cell points, if that cell still shows the link.
    ///
    /// Takes `(row, column)` in screen cells. Returns nothing while the pane is scrolled
    /// back, because links are recorded against the live screen and would line up with
    /// the wrong rows of history.
    pub fn link_at(&self, screen: &vt100::Screen, row: u16, column: u16) -> Option<String> {
        self.live_links(screen)
            .find(|link| (row, column) >= link.start && (row, column) < link.end)
            .map(|link| link.target.clone())
    }

    /// Every link still showing on the live screen, as `(start, end, target)` cells.
    ///
    /// The same rule as [`Self::link_at`], so what gets drawn as a hyperlink and what a
    /// click can reach never disagree.
    pub fn links_on_screen(&self, screen: &vt100::Screen) -> Vec<LiveLink> {
        self.live_links(screen)
            .map(|link| LiveLink {
                start: link.start,
                end: link.end,
                target: link.target.clone(),
            })
            .collect()
    }

    /// Links whose cells still read what they read when drawn, newest first.
    ///
    /// Nothing while scrolled back through history: links are recorded against the live
    /// screen, and scrolled back the same cells show older rows.
    fn live_links<'a>(
        &'a self,
        screen: &'a vt100::Screen,
    ) -> impl Iterator<Item = &'a LinkSpan> + 'a {
        let live = screen.scrollback() == 0;
        let alternate = screen.alternate_screen();
        self.links.iter().rev().filter(move |link| {
            live && link.alternate == alternate
                && screen.contents_between(link.start.0, link.start.1, link.end.0, link.end.1)
                    == link.text
        })
    }

    /// Follow an OSC 8 sequence: `8 ; params ; target` opens a link, an empty target
    /// closes it.
    fn hyperlink(&mut self, screen: &vt100::Screen, params: &[&[u8]]) {
        // A target can itself contain `;`, which the parser has already split on, so
        // everything after the parameters field is put back together.
        let target = params.get(2..).unwrap_or_default().join(&b';');
        let target = String::from_utf8_lossy(&target).into_owned();
        let here = screen.cursor_position();
        // Whatever was open ends here — either this sequence closes it, or a new link
        // starts without the old one having been closed, which some programs do.
        if let Some(open) = self.open_link.take() {
            self.record_link(screen, open, here);
        }
        if !target.is_empty() {
            self.open_link = Some(OpenLink {
                start: here,
                alternate: screen.alternate_screen(),
                target,
            });
        }
    }

    fn record_link(&mut self, screen: &vt100::Screen, open: OpenLink, end: (u16, u16)) {
        if end <= open.start || open.alternate != screen.alternate_screen() {
            return;
        }
        let text = screen.contents_between(open.start.0, open.start.1, end.0, end.1);
        if text.trim().is_empty() {
            return;
        }
        // The child redraws its view constantly and sends the same link each time. The
        // newer copy replaces the older one rather than piling up behind it.
        self.links
            .retain(|link| link.start != open.start || link.alternate != open.alternate);
        self.links.push_back(LinkSpan {
            start: open.start,
            end,
            alternate: open.alternate,
            text,
            target: open.target,
        });
        while self.links.len() > MAX_LINKS {
            self.links.pop_front();
        }
    }

    /// Window title reported by the child via OSC 0/2, if any.
    pub fn take_title(&mut self) -> Option<String> {
        self.title.take()
    }

    pub fn take_signals(&mut self) -> PaneSignals {
        PaneSignals {
            title: self.take_title(),
            events: std::mem::take(&mut self.signals),
        }
    }

    fn reply(&self, bytes: Vec<u8>) {
        let _ = self.replies.send(bytes);
    }

    pub fn set_terminal_light_mode(&mut self, terminal_light_mode: Option<bool>) {
        if self.terminal_light_mode == terminal_light_mode {
            return;
        }
        self.terminal_light_mode = terminal_light_mode;
        if self.theme_updates_requested {
            if let Some(light_theme) = terminal_light_mode {
                self.reply(theme_report(light_theme));
            }
        }
    }
}

fn theme_report(light_theme: bool) -> Vec<u8> {
    format!("\x1b[?997;{}n", if light_theme { 2 } else { 1 }).into_bytes()
}

impl vt100::Callbacks for PaneCallbacks {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, selector: &[u8], data: &[u8]) {
        let mut sequence = Vec::with_capacity(selector.len() + data.len() + 10);
        sequence.extend_from_slice(b"\x1b]52;");
        sequence.extend_from_slice(selector);
        sequence.push(b';');
        sequence.extend_from_slice(data);
        sequence.extend_from_slice(b"\x1b\\");
        let _ = self
            .events
            .send(MuxEvent::HostSequence(self.pane_id, sequence));
    }

    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.signals.push(PaneSignalEvent::Bell);
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).to_string());
    }

    fn set_window_icon_name(&mut self, _: &mut vt100::Screen, name: &[u8]) {
        if self.title.is_none() {
            self.title = Some(String::from_utf8_lossy(name).to_string());
        }
    }

    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        if params.first() == Some(&&b"8"[..]) {
            // Recorded, never forwarded. Passing a child's OSC through to the outer
            // terminal is exactly what the progress passthrough below refuses to do.
            self.hyperlink(screen, params);
            return;
        }
        if let Some(progress) = crate::host_terminal::progress_state(params) {
            self.signals.push(PaneSignalEvent::Progress(progress));
        }
        if let Some(sequence) = crate::host_terminal::progress_sequence(params) {
            let _ = self
                .events
                .send(MuxEvent::HostSequence(self.pane_id, sequence));
        }
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        intermediate: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        final_byte: char,
    ) {
        let first = params.first().and_then(|group| group.first()).copied();
        match (final_byte, intermediate) {
            // DSR — device status report.
            ('n', None) => match first {
                // Terminal is OK.
                Some(5) => self.reply(b"\x1b[0n".to_vec()),
                // Cursor position report, 1-based.
                Some(6) => {
                    let (row, col) = screen.cursor_position();
                    self.reply(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes());
                }
                _ => {}
            },
            // Report the appearance of CST's nested terminal rather than the host
            // operating-system theme. Copilot's `github` theme uses this response.
            ('n', Some(b'?')) if first == Some(996) => {
                if let Some(light_theme) = self.terminal_light_mode {
                    self.reply(theme_report(light_theme));
                }
            }
            // Applications may request an unsolicited report when the palette changes.
            ('h', Some(b'?')) if first == Some(2031) => {
                self.theme_updates_requested = true;
            }
            ('l', Some(b'?')) if first == Some(2031) => {
                self.theme_updates_requested = false;
            }
            // DA1 — primary device attributes: claim a VT220 with 132-column and
            // selective-erase support, which is what xterm-compatible apps expect.
            ('c', None) => self.reply(b"\x1b[?62;1;6c".to_vec()),
            // DA2 — secondary device attributes: report as xterm.
            ('c', Some(b'>')) => self.reply(b"\x1b[>0;10;1c".to_vec()),
            // XTWINOPS: report text area size in characters.
            ('t', None) if first == Some(18) => {
                let (rows, cols) = screen.size();
                self.reply(format!("\x1b[8;{};{}t", rows, cols).into_bytes());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn parser_with_replies(
        light_theme: bool,
    ) -> (
        vt100::Parser<PaneCallbacks>,
        mpsc::Receiver<Vec<u8>>,
        mpsc::Receiver<MuxEvent>,
    ) {
        let (tx, rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        (
            vt100::Parser::new_with_callbacks(
                24,
                80,
                0,
                PaneCallbacks::new(7, tx, event_tx, Some(light_theme)),
            ),
            rx,
            event_rx,
        )
    }

    #[test]
    fn answers_cursor_position_requests() {
        let (mut parser, rx, _) = parser_with_replies(false);

        // Move to row 3, col 5 (1-based), then ask where the cursor is.
        parser.process(b"\x1b[3;5H\x1b[6n");

        let reply = rx.try_recv().unwrap();
        assert_eq!(reply, b"\x1b[3;5R");
    }

    #[test]
    fn answers_device_status_and_attribute_queries() {
        let (mut parser, rx, _) = parser_with_replies(false);

        parser.process(b"\x1b[5n");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[0n");

        parser.process(b"\x1b[c");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[?62;1;6c");

        parser.process(b"\x1b[>c");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[>0;10;1c");
    }

    #[test]
    fn reports_the_text_area_size() {
        let (mut parser, rx, _) = parser_with_replies(false);

        parser.process(b"\x1b[18t");

        assert_eq!(rx.try_recv().unwrap(), b"\x1b[8;24;80t");
    }

    #[test]
    fn captures_the_window_title() {
        let (mut parser, _rx, _) = parser_with_replies(false);

        parser.process(b"\x1b]2;my session\x07");

        assert_eq!(
            parser.callbacks_mut().take_title().as_deref(),
            Some("my session")
        );
    }

    #[test]
    fn unrelated_sequences_produce_no_reply() {
        let (mut parser, rx, _) = parser_with_replies(false);

        parser.process(b"hello\x1b[1;1H");

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn forwards_clipboard_requests_to_the_host() {
        let (mut parser, _, events) = parser_with_replies(false);

        parser.process(b"\x1b]52;c;Q29waWVkIHRleHQ=\x07");

        let MuxEvent::HostSequence(id, sequence) = events.try_recv().unwrap() else {
            panic!("expected host sequence");
        };
        assert_eq!(id, 7);
        assert_eq!(sequence, b"\x1b]52;c;Q29waWVkIHRleHQ=\x1b\\");
    }

    #[test]
    fn forwards_progress_state_to_the_host() {
        let (mut parser, _, events) = parser_with_replies(false);

        parser.process(b"\x1b]9;4;3;0\x07");

        let MuxEvent::HostSequence(id, sequence) = events.try_recv().unwrap() else {
            panic!("expected host sequence");
        };
        assert_eq!(id, 7);
        assert_eq!(sequence, b"\x1b]9;4;3;0\x1b\\");
        assert_eq!(
            parser.callbacks_mut().take_signals().events,
            vec![PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Indeterminate
            )]
        );
    }

    #[test]
    fn progress_signals_are_drained_per_output_chunk() {
        let (mut parser, _, _) = parser_with_replies(false);

        parser.process(b"\x1b]9;4;3;0\x07");
        let working = parser.callbacks_mut().take_signals();
        parser.process(b"\x1b]9;4;0;0\x07");
        let complete = parser.callbacks_mut().take_signals();

        assert_eq!(
            working.events,
            vec![PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Indeterminate
            )]
        );
        assert_eq!(
            complete.events,
            vec![PaneSignalEvent::Progress(
                crate::host_terminal::ProgressState::Clear
            )]
        );
        assert!(parser.callbacks_mut().take_signals().events.is_empty());
    }

    fn link_under(parser: &vt100::Parser<PaneCallbacks>, row: u16, column: u16) -> Option<String> {
        parser.callbacks().link_at(parser.screen(), row, column)
    }

    /// The report: links drawn as text with nothing behind them.
    ///
    /// A named link carries its target in the escape sequence, not the text, so once the
    /// sequence was dropped there was nothing left for anyone to open.
    #[test]
    fn a_named_link_is_found_under_its_text_and_nowhere_else() {
        let (mut parser, _, events) = parser_with_replies(false);
        parser.process(
            b"see \x1b]8;;file:///D:/gallery/index.html\x1b\\Open the local gallery\x1b]8;;\x1b\\.",
        );

        let target = Some("file:///D:/gallery/index.html".to_string());
        assert_eq!(link_under(&parser, 0, 4), target, "its first letter");
        assert_eq!(link_under(&parser, 0, 25), target, "its last letter");
        assert_eq!(link_under(&parser, 0, 3), None, "the space before it");
        assert_eq!(link_under(&parser, 0, 26), None, "the full stop after it");
        assert!(
            events.try_recv().is_err(),
            "recorded, never passed on to the outer terminal"
        );
    }

    #[test]
    fn a_link_drawn_over_is_no_longer_there_to_click() {
        let (mut parser, _, _) = parser_with_replies(false);
        parser.process(b"\x1b]8;;https://a.example\x1b\\first\x1b]8;;\x1b\\");
        assert!(link_under(&parser, 0, 0).is_some());

        // The child redraws that line with something else, and no link this time.
        parser.process(b"\x1b[1;1Hplain");

        assert_eq!(
            link_under(&parser, 0, 0),
            None,
            "a click must never open a link that is no longer on screen"
        );
    }

    #[test]
    fn a_target_containing_semicolons_arrives_whole() {
        // The parser splits OSC parameters on `;`, which a URL may legitimately contain.
        let (mut parser, _, _) = parser_with_replies(false);
        parser.process(b"\x1b]8;;https://example.com/a;b=1;c\x1b\\x\x1b]8;;\x1b\\");

        assert_eq!(
            link_under(&parser, 0, 0).as_deref(),
            Some("https://example.com/a;b=1;c")
        );
    }

    #[test]
    fn a_link_that_wraps_can_be_clicked_on_either_row() {
        let (mut parser, _, _) = parser_with_replies(false);
        // Eight letters starting four from the right edge: half on each row.
        parser.process(b"\x1b[1;77H\x1b]8;;https://w.example\x1b\\abcdefgh\x1b]8;;\x1b\\");

        assert!(link_under(&parser, 0, 76).is_some(), "the first half");
        assert!(link_under(&parser, 1, 3).is_some(), "the second half");
        assert_eq!(link_under(&parser, 1, 4), None, "just past the end");
    }

    #[test]
    fn a_link_redrawn_every_frame_is_remembered_once() {
        // Copilot repaints its whole view constantly. Every repaint resends the links.
        let (mut parser, _, _) = parser_with_replies(false);
        for _ in 0..1000 {
            parser.process(b"\x1b[1;1H\x1b]8;;https://a.example\x1b\\link\x1b]8;;\x1b\\");
        }
        assert_eq!(parser.callbacks().links.len(), 1);
    }

    #[test]
    fn a_new_link_closes_one_left_open() {
        // Some programs start the next link without closing the last.
        let (mut parser, _, _) = parser_with_replies(false);
        parser.process(
            b"\x1b]8;;https://a.example\x1b\\one \x1b]8;;https://b.example\x1b\\two\x1b]8;;\x1b\\",
        );

        assert_eq!(
            link_under(&parser, 0, 0).as_deref(),
            Some("https://a.example")
        );
        assert_eq!(
            link_under(&parser, 0, 4).as_deref(),
            Some("https://b.example")
        );
    }

    #[test]
    fn nothing_resolves_while_scrolled_back_through_history() {
        // Links are recorded against the live screen. Scrolled back, the same cells show
        // older rows, and a click would open whatever link used to sit there.
        let (tx, _) = mpsc::channel();
        let (event_tx, _) = mpsc::channel();
        let mut parser = vt100::Parser::new_with_callbacks(
            24,
            80,
            100,
            PaneCallbacks::new(7, tx, event_tx, Some(false)),
        );
        for line in 0..40 {
            parser.process(format!("line {line}\r\n").as_bytes());
        }
        parser.process(b"\x1b]8;;https://a.example\x1b\\link\x1b]8;;\x1b\\");
        let (row, _) = parser.screen().cursor_position();
        assert!(link_under(&parser, row, 0).is_some());

        parser.screen_mut().set_scrollback(5);

        assert_eq!(link_under(&parser, row, 0), None);
    }

    #[test]
    fn does_not_forward_unrelated_osc_commands() {
        let (mut parser, _, events) = parser_with_replies(false);

        parser.process(b"\x1b]8;;https://example.com\x07");

        assert!(events.try_recv().is_err());
    }

    #[test]
    fn reports_the_nested_cst_theme_to_copilot() {
        let (mut dark, dark_replies, _) = parser_with_replies(false);
        dark.process(b"\x1b[?996n");
        assert_eq!(dark_replies.try_recv().unwrap(), b"\x1b[?997;1n");

        let (mut light, light_replies, _) = parser_with_replies(true);
        light.process(b"\x1b[?996n");
        assert_eq!(light_replies.try_recv().unwrap(), b"\x1b[?997;2n");
    }

    #[test]
    fn unspecified_classic_theme_leaves_detection_to_the_host_environment() {
        let (tx, replies) = mpsc::channel();
        let (events, _) = mpsc::channel();
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, PaneCallbacks::new(7, tx, events, None));

        parser.process(b"\x1b[?996n");

        assert!(replies.try_recv().is_err());
    }

    #[test]
    fn subscribed_children_receive_only_real_appearance_changes() {
        let (mut parser, replies, _) = parser_with_replies(false);
        parser.process(b"\x1b[?2031h");

        parser.callbacks_mut().set_terminal_light_mode(Some(true));
        assert_eq!(replies.try_recv().unwrap(), b"\x1b[?997;2n");
        parser.callbacks_mut().set_terminal_light_mode(Some(true));
        assert!(replies.try_recv().is_err());

        parser.process(b"\x1b[?2031l");
        parser.callbacks_mut().set_terminal_light_mode(Some(false));
        assert!(replies.try_recv().is_err());
    }
}
