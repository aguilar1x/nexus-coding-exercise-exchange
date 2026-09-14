//! Terminal UI for the matching engine.
//!
//! This lives under the binary rather than in `src/engine/` on purpose: the
//! library stays free of any UI dependency, so the matcher and its tests never
//! link ratatui and never need a terminal.
//!
//! `render` is a free function over a `View`, not a method on a terminal, so it
//! can be exercised against ratatui's `TestBackend` with no tty at all.

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use services::engine::matcher::Engine;
use services::engine::types::{Price, Qty, Symbol};
use services::feed::OrderId;
use std::collections::VecDeque;
use std::io;
use std::time::Duration;

/// How many recent trades the tape shows.
pub const TAPE_CAPACITY: usize = 32;
/// Price levels per side in each ladder.
pub const DEPTH_LEVELS: usize = 6;
/// Below this many columns the per-symbol panels cannot sit side by side.
pub const NARROW_WIDTH: u16 = 100;

const GREEN: Color = Color::Green;
const RED: Color = Color::Red;
const DIM: Color = Color::DarkGray;

/// One line of the trade tape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeEntry {
    pub seq: OrderId,
    pub symbol: Symbol,
    pub price: Price,
    pub qty: Qty,
    /// True when the aggressor was buying, which colours the row.
    pub taker_bought: bool,
}

/// Recent trades, newest first.
///
/// The engine reports trades as they happen and then forgets them — it holds
/// books, not history — so the tape window belongs to the UI.
#[derive(Debug)]
pub struct TapeBuffer {
    entries: VecDeque<TapeEntry>,
    capacity: usize,
}

impl TapeBuffer {
    pub fn new(capacity: usize) -> Self {
        TapeBuffer {
            entries: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// Adds a trade, evicting the oldest once the window is full.
    pub fn push(&mut self, entry: TapeEntry) {
        self.entries.push_front(entry);
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
    }

    /// Newest first.
    pub fn iter(&self) -> impl Iterator<Item = &TapeEntry> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Everything a frame needs. Borrowed, so rendering allocates nothing durable.
pub struct View<'a> {
    pub engine: &'a Engine,
    pub tape: &'a TapeBuffer,
    pub url: &'a str,
    pub cursor: OrderId,
    pub connected: bool,
    pub filter: Option<Symbol>,
    pub skipped: u64,
}

/// Enters the alternate screen. `ratatui::try_init` also installs a panic hook
/// that restores the terminal first, so a crash cannot leave the shell unusable.
///
/// Returns the error rather than panicking: without a tty — piped to a file, or
/// under CI — `init` would abort with a raw OS error, and the caller can give a
/// useful message and point at the plain renderer instead.
pub fn enter() -> io::Result<DefaultTerminal> {
    ratatui::try_init()
}

pub fn leave() {
    ratatui::restore();
}

/// Non-blocking keyboard check. Returns true when the user asked to quit.
pub fn quit_requested() -> io::Result<bool> {
    if !event::poll(Duration::ZERO)? {
        return Ok(false);
    }
    match event::read()? {
        Event::Key(key) if key.kind == KeyEventKind::Press => Ok(matches!(
            key.code,
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
        )),
        _ => Ok(false),
    }
}

pub fn render(frame: &mut Frame, view: &View) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(6),
    ])
    .areas(frame.area());

    render_header(frame, header, view);
    render_body(frame, body, view);
    render_footer(frame, footer, view);
}

fn render_header(frame: &mut Frame, area: Rect, view: &View) {
    let (status, status_style) = if view.connected {
        ("connected", Style::default().fg(GREEN))
    } else {
        (
            "RECONNECTING",
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )
    };
    let line = Line::from(vec![
        Span::raw(format!("{}  ", view.url)),
        Span::styled(status, status_style),
        Span::raw(format!("   cursor {}", view.cursor)),
        Span::raw(format!("   filter {}", view.filter.unwrap_or("all"))),
        Span::styled("   [q] quit", Style::default().fg(DIM)),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL).title(" engine ")),
        area,
    );
}

fn render_body(frame: &mut Frame, area: Rect, view: &View) {
    let mut symbols: Vec<Symbol> = view.engine.symbols().copied().collect();
    symbols.sort_unstable();

    // Narrow terminals cannot fit the panels side by side, so stack instead.
    let (books_area, tape_area) = if area.width >= NARROW_WIDTH {
        let [books, tape] =
            Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)])
                .areas(area);
        (books, tape)
    } else {
        let [books, tape] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(10)]).areas(area);
        (books, tape)
    };

    if symbols.is_empty() {
        frame.render_widget(
            Paragraph::new("waiting for the first order...")
                .block(Block::default().borders(Borders::ALL).title(" books ")),
            books_area,
        );
    } else {
        // Side by side when there is room, stacked when there is not.
        let slots = vec![Constraint::Ratio(1, symbols.len() as u32); symbols.len()];
        let areas: Vec<Rect> = if books_area.width >= NARROW_WIDTH {
            Layout::horizontal(slots).split(books_area).to_vec()
        } else {
            Layout::vertical(slots).split(books_area).to_vec()
        };
        for (symbol, slot) in symbols.iter().zip(areas) {
            render_symbol(frame, slot, view, symbol);
        }
    }

    render_tape(frame, tape_area, view);
}

fn render_symbol(frame: &mut Frame, area: Rect, view: &View, symbol: &Symbol) {
    let mut lines = Vec::new();

    match view.engine.market.symbol(symbol) {
        Some(m) => {
            lines.push(Line::from(vec![
                Span::raw("last "),
                Span::styled(
                    fmt_price(m.last),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  vwap {}", fmt_price(m.vwap()))),
            ]));
            lines.push(Line::styled(
                format!(
                    "hi {}  lo {}  vol {}  ({} trades)",
                    fmt_price(m.high),
                    fmt_price(m.low),
                    Qty(m.volume),
                    m.trades
                ),
                Style::default().fg(DIM),
            ));
        }
        None => lines.push(Line::styled("no trades yet", Style::default().fg(DIM))),
    }

    lines.push(Line::styled(
        format!("{:>13} | {:<13}", "BIDS", "ASKS"),
        Style::default().fg(DIM).add_modifier(Modifier::BOLD),
    ));

    if let Some(book) = view.engine.book(symbol) {
        let (bids, asks) = book.depth(DEPTH_LEVELS);
        for i in 0..DEPTH_LEVELS {
            let bid = bids
                .get(i)
                .map(|(p, q)| format!("{} x {}", p, q))
                .unwrap_or_default();
            let ask = asks
                .get(i)
                .map(|(p, q)| format!("{} x {}", p, q))
                .unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(format!("{:>13}", bid), Style::default().fg(GREEN)),
                Span::styled(" | ", Style::default().fg(DIM)),
                Span::styled(format!("{:<13}", ask), Style::default().fg(RED)),
            ]));
        }
        lines.push(Line::styled(
            format!("{} resting", book.len()),
            Style::default().fg(DIM),
        ));
    }

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", symbol)),
        ),
        area,
    );
}

fn render_tape(frame: &mut Frame, area: Rect, view: &View) {
    if view.tape.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "waiting for the first trade...",
                Style::default().fg(DIM),
            ))
            .block(Block::default().borders(Borders::ALL).title(" trades ")),
            area,
        );
        return;
    }

    let lines: Vec<Line> = view
        .tape
        .iter()
        .map(|t| {
            let (mark, colour) = if t.taker_bought {
                ("B", GREEN)
            } else {
                ("S", RED)
            };
            Line::from(vec![
                Span::styled(format!("#{:<7}", t.seq), Style::default().fg(DIM)),
                Span::raw(format!("{:<9} ", t.symbol)),
                Span::styled(format!("{:>9}", t.price), Style::default().fg(colour)),
                Span::raw(format!(" x {:>5} ", t.qty)),
                Span::styled(
                    mark,
                    Style::default().fg(colour).add_modifier(Modifier::BOLD),
                ),
            ])
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" trades ({}) ", view.tape.len())),
        ),
        area,
    );
}

fn render_footer(frame: &mut Frame, area: Rect, view: &View) {
    let s = &view.engine.stats;
    let mut lines = vec![
        Line::from(format!(
            "msgs {}  new {}  filtered {}  cancels {} (hit {} / unknown {})  skipped {}",
            s.events,
            s.new_orders,
            s.filtered,
            s.cancels,
            s.cancels_hit,
            s.cancels_unknown,
            view.skipped,
        )),
        Line::from(format!(
            "trades {}  volume {}  cancelled {}  resting {}  indexed {}  net {}",
            s.trades,
            Qty(s.volume),
            Qty(s.cancelled_qty),
            view.engine.resting(),
            view.engine.indexed(),
            Qty(view.engine.market.net_position()),
        )),
    ];

    let positions = view.engine.market.top_positions(4);
    if !positions.is_empty() {
        let text = positions
            .iter()
            .map(|(account, symbol, qty)| {
                let sign = if qty.0 > 0 { "+" } else { "" };
                format!("a{} {} {}{}", account, symbol, sign, qty)
            })
            .collect::<Vec<_>>()
            .join("  |  ");
        lines.push(Line::styled(
            format!("positions: {}", text),
            Style::default().fg(DIM),
        ));
    }

    // The same invariants the plain renderer checks, surfaced where they cannot
    // scroll away.
    let mut broken = Vec::new();
    if view.engine.any_crossed() {
        broken.push("book crossed");
    }
    if view.engine.indexed() != view.engine.resting() {
        broken.push("index drifted");
    }
    if view.engine.market.net_position() != 0 {
        broken.push("positions do not net to zero");
    }
    lines.push(if broken.is_empty() {
        Line::styled("invariants ok", Style::default().fg(GREEN))
    } else {
        Line::styled(
            format!("INVARIANT VIOLATED: {}", broken.join(", ")),
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        )
    });

    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn fmt_price(p: Option<Price>) -> String {
    p.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use services::engine::types::{Event, Order};
    use services::feed::Side;

    fn entry(seq: OrderId) -> TapeEntry {
        TapeEntry {
            seq,
            symbol: "ETH-USDC",
            price: Price(10000),
            qty: Qty(50),
            taker_bought: true,
        }
    }

    #[test]
    fn the_tape_shows_the_newest_trade_first() {
        let mut tape = TapeBuffer::new(8);
        tape.push(entry(1));
        tape.push(entry(2));
        tape.push(entry(3));
        let seqs: Vec<_> = tape.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![3, 2, 1]);
    }

    #[test]
    fn the_tape_evicts_the_oldest_once_it_is_full() {
        let mut tape = TapeBuffer::new(3);
        for seq in 1..=5 {
            tape.push(entry(seq));
        }
        assert_eq!(tape.len(), 3);
        let seqs: Vec<_> = tape.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![5, 4, 3], "should keep the most recent three");
    }

    #[test]
    fn a_zero_capacity_tape_still_holds_one_entry() {
        // Guards against a `%` or `while` on a zero bound silently dropping
        // everything, or panicking.
        let mut tape = TapeBuffer::new(0);
        tape.push(entry(1));
        assert_eq!(tape.len(), 1);
    }

    #[test]
    fn a_new_tape_is_empty() {
        let tape = TapeBuffer::new(4);
        assert!(tape.is_empty());
        assert_eq!(tape.len(), 0);
    }

    fn engine_with_a_trade() -> Engine {
        let mut engine = Engine::new();
        let order = |id, side, price, qty| Event::New {
            symbol: "ETH-USDC",
            order: Order {
                id,
                account: 1,
                side,
                price: Price(price),
                qty: Qty(qty),
            },
        };
        engine.apply(&order(1, Side::Sell, 10000, 50));
        engine.apply(&order(2, Side::Buy, 10000, 20));
        engine
    }

    fn render_at(width: u16, height: u16, engine: &Engine, tape: &TapeBuffer) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let view = View {
            engine,
            tape,
            url: "http://127.0.0.1:3000",
            cursor: 42,
            connected: true,
            filter: None,
            skipped: 0,
        };
        terminal.draw(|frame| render(frame, &view)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn a_full_frame_renders_without_panicking() {
        let engine = engine_with_a_trade();
        let mut tape = TapeBuffer::new(TAPE_CAPACITY);
        tape.push(entry(2));
        let out = render_at(120, 40, &engine, &tape);
        assert!(out.contains("ETH-USDC"));
        assert!(out.contains("127.0.0.1"));
        assert!(out.contains("invariants ok"));
    }

    #[test]
    fn it_survives_a_terminal_too_narrow_for_side_by_side_panels() {
        // The layout switches to stacked below NARROW_WIDTH; a panic here would
        // take down the whole runner.
        let engine = engine_with_a_trade();
        let tape = TapeBuffer::new(TAPE_CAPACITY);
        for (w, h) in [(40, 20), (20, 10), (1, 1)] {
            let out = render_at(w, h, &engine, &tape);
            assert!(!out.is_empty(), "{}x{} produced nothing", w, h);
        }
    }

    #[test]
    fn an_engine_with_no_orders_yet_renders_a_placeholder() {
        let engine = Engine::new();
        let tape = TapeBuffer::new(TAPE_CAPACITY);
        let out = render_at(120, 40, &engine, &tape);
        assert!(out.contains("waiting for the first order"));
        assert!(out.contains("waiting for the first trade"));
    }
}
