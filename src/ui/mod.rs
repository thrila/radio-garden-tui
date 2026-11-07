//! Rendering. Every function here is pure: it reads `&App` and draws into a
//! `Frame`; no state is mutated and no I/O happens.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    symbols::border,
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Table, TableState, Wrap,
        canvas::{Canvas, Map, MapResolution, Points},
    },
};

use crate::app::{App, ListState, Notice, Playback, Screen, SearchFocus, SearchItem};

// ---------------------------------------------------------------------------
// Palette: phosphor green CRT
// ---------------------------------------------------------------------------

const BG: Color = Color::Rgb(4, 8, 4);
const BG_ALT: Color = Color::Rgb(8, 16, 8);
const GREEN: Color = Color::Rgb(0, 255, 65);
const MID: Color = Color::Rgb(0, 190, 50);
const DIM: Color = Color::Rgb(0, 120, 35);
const DARK: Color = Color::Rgb(0, 60, 18);
const CYAN: Color = Color::Rgb(0, 230, 200);
const AMBER: Color = Color::Rgb(255, 190, 0);
const RED: Color = Color::Rgb(255, 60, 60);
const WHITE: Color = Color::Rgb(210, 255, 210);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

// The main logo, "RADIOGARDEN" in the figlet "ANSI Shadow" font.
const SHADOW_LOGO: [&str; 6] = [
    "██████╗  █████╗ ██████╗ ██╗ ██████╗  ██████╗  █████╗ ██████╗ ██████╗ ███████╗███╗   ██╗",
    "██╔══██╗██╔══██╗██╔══██╗██║██╔═══██╗██╔════╝ ██╔══██╗██╔══██╗██╔══██╗██╔════╝████╗  ██║",
    "██████╔╝███████║██║  ██║██║██║   ██║██║  ███╗███████║██████╔╝██║  ██║█████╗  ██╔██╗ ██║",
    "██╔══██╗██╔══██║██║  ██║██║██║   ██║██║   ██║██╔══██║██╔══██╗██║  ██║██╔══╝  ██║╚██╗██║",
    "██║  ██║██║  ██║██████╔╝██║╚██████╔╝╚██████╔╝██║  ██║██║  ██║██████╔╝███████╗██║ ╚████║",
    "╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝ ╚═╝ ╚═════╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝ ╚══════╝╚═╝  ╚═══╝",
];

// Fallback for narrower terminals: figlet "small", assembled per glyph.
// Every row of a glyph has the same width so the banner always lines up.
const SMALL_GLYPHS: &[(char, [&str; 4])] = &[
    ('R', [r" ___ ", r"| _ \", r"|   /", r"|_|_\"]),
    ('A', [r"   _   ", r"  /_\  ", r" / _ \ ", r"/_/ \_\"]),
    ('D', [r" ___  ", r"|   \ ", r"| |) |", r"|___/ "]),
    ('I', [r" ___ ", r"|_ _|", r" | | ", r"|___|"]),
    ('O', [r"  ___  ", r" / _ \ ", r"| (_) |", r" \___/ "]),
    ('G', [r"  ___ ", r" / __|", r"| (_ |", r" \___|"]),
    ('E', [r" ___ ", r"| __|", r"| _| ", r"|___|"]),
    ('N', [r" _  _ ", r"| \| |", r"| .` |", r"|_|\_|"]),
    (' ', [r"  ", r"  ", r"  ", r"  "]),
];
const BANNER_TEXT: &str = "RADIO GARDEN";
const BANNER_ROWS: u16 = 6;
const MIN_HEIGHT_FOR_BANNER: u16 = 30;
/// Room the [sys]/[net]/... readout needs beside the logo.
const BANNER_INFO_WIDTH: u16 = 38;

fn assemble<const N: usize>(font: &[(char, [&str; N])]) -> [String; N] {
    let mut rows: [String; N] = std::array::from_fn(|_| String::new());
    for ch in BANNER_TEXT.chars() {
        if let Some((_, glyph)) = font.iter().find(|(c, _)| *c == ch) {
            for (row, part) in rows.iter_mut().zip(glyph) {
                row.push_str(part);
            }
        }
    }
    rows
}

fn width_of<S: AsRef<str>>(rows: &[S]) -> u16 {
    rows.first()
        .map_or(0, |r| r.as_ref().chars().count() as u16)
}

/// Colour an ANSI Shadow row: solid blocks lit, box-drawing "shadow" dim.
fn shadow_line(row: &str, face: Color) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let mut run = String::new();
    let mut run_is_face = true;
    for ch in row.chars() {
        let is_face = ch == '█' || ch == ' ';
        if is_face != run_is_face && !run.is_empty() {
            let style = if run_is_face { bold(face) } else { s(DARK) };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_is_face = is_face;
        run.push(ch);
    }
    if !run.is_empty() {
        let style = if run_is_face { bold(face) } else { s(DARK) };
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

fn banner_height(term_height: u16) -> u16 {
    if term_height >= MIN_HEIGHT_FOR_BANNER {
        BANNER_ROWS
    } else {
        0
    }
}

const PROMPT_ROWS: u16 = 1;
const PLAYER_ROWS: u16 = 3;
const FOOTER_ROWS: u16 = 1;
const SEARCH_INPUT_ROWS: u16 = 3;
/// Border top/bottom + table header row.
const LIST_CHROME_ROWS: u16 = 3;

/// Number of list rows visible on `screen` for a terminal of `term_height`.
/// `main` feeds this to the app so paging/scrolling matches what's drawn.
pub fn list_rows(term_height: u16, screen: Screen) -> usize {
    let mut used =
        banner_height(term_height) + PROMPT_ROWS + PLAYER_ROWS + FOOTER_ROWS + LIST_CHROME_ROWS;
    if screen == Screen::Search {
        used += SEARCH_INPUT_ROWS;
    }
    term_height.saturating_sub(used).max(1) as usize
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn s(fg: Color) -> Style {
    Style::default().fg(fg)
}

fn bold(fg: Color) -> Style {
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

fn panel<'a>(title: impl Into<String>, focused: bool) -> Block<'a> {
    let title: String = title.into();
    let (edge, label) = if focused { (MID, GREEN) } else { (DARK, DIM) };
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::PLAIN)
        .border_style(s(edge))
        .title(Line::from(vec![
            Span::styled("┤ ", s(edge)),
            Span::styled(title, bold(label)),
            Span::styled(" ├", s(edge)),
        ]))
        .style(Style::default().bg(BG))
}

fn spinner(app: &App) -> &'static str {
    SPINNER[(app.tick as usize) % SPINNER.len()]
}

fn blink(app: &App) -> bool {
    (app.tick / 6).is_multiple_of(2)
}

fn hms(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

fn volume_bar(volume: f32, width: usize) -> String {
    let filled = ((volume * width as f32).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

/// Status tag in the style of an init system: `[  OK  ]`, `[ FAIL ]`, ...
fn status_tag(app: &App) -> (String, Style) {
    let (text, style) = match &app.playback {
        Playback::Idle => ("IDLE", s(DIM)),
        Playback::Connecting => ("LINK", bold(AMBER)),
        Playback::Buffering => ("BUFF", bold(AMBER)),
        Playback::Playing => ("LIVE", bold(GREEN)),
        Playback::Stopped => ("HALT", s(DIM)),
        Playback::Ended => ("EOF ", bold(AMBER)),
        Playback::Error(_) => ("FAIL", bold(RED)),
    };
    (format!("[ {text} ]"), style)
}

fn playback_label(app: &App) -> String {
    match &app.playback {
        Playback::Idle => "no carrier".into(),
        Playback::Connecting => format!("{} resolving stream...", spinner(app)),
        Playback::Buffering => format!("{} buffering...", spinner(app)),
        Playback::Playing => "receiving".into(),
        Playback::Stopped => "halted".into(),
        Playback::Ended => "stream closed by remote host".into(),
        Playback::Error(e) => e.clone(),
    }
}

/// Scale list size into a small bar so busy places stand out.
fn size_bar(size: u32) -> String {
    let level = match size {
        0..=1 => 1,
        2..=4 => 2,
        5..=14 => 3,
        15..=49 => 4,
        50..=149 => 5,
        _ => 6,
    };
    format!("{}{}", "▮".repeat(level), "▯".repeat(6 - level))
}

/// Cheap deterministic hash for animations.
fn hash(a: u64, b: u64) -> u64 {
    let mut x = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^ (x >> 29)
}

// ---------------------------------------------------------------------------
// top level
// ---------------------------------------------------------------------------

pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BG).fg(GREEN)),
        area,
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(banner_height(area.height)),
            Constraint::Length(PROMPT_ROWS),
            Constraint::Min(3),
            Constraint::Length(PLAYER_ROWS),
            Constraint::Length(FOOTER_ROWS),
        ])
        .split(area);

    if chunks[0].height > 0 {
        render_banner(frame, app, chunks[0]);
    }
    render_breadcrumb(frame, app, chunks[1]);
    match app.screen {
        Screen::Places => render_places(frame, app, chunks[2]),
        Screen::Stations => render_stations(frame, app, chunks[2]),
        Screen::Search => render_search(frame, app, chunks[2]),
        Screen::NowPlaying => render_now_playing(frame, app, chunks[2]),
    }
    render_player_bar(frame, app, chunks[3]);
    render_footer(frame, app, chunks[4]);

    if app.show_help {
        render_help(frame, area);
    }
}

fn render_banner(frame: &mut Frame, app: &App, area: Rect) {
    let shadow = SHADOW_LOGO;
    let small = assemble(SMALL_GLYPHS);

    let (lines, art_width): (Vec<Line>, u16) = if area.width >= width_of(&shadow) + 2 {
        let shades = [GREEN, GREEN, GREEN, MID, MID, DIM];
        (
            shadow
                .iter()
                .zip(shades)
                .map(|(row, c)| shadow_line(row, c))
                .collect(),
            width_of(&shadow) + 2,
        )
    } else if area.width >= width_of(&small) + 2 {
        let shades = [GREEN, GREEN, MID, DIM];
        let mut lines = vec![Line::raw("")];
        lines.extend(
            small
                .iter()
                .zip(shades)
                .map(|(row, c)| Line::from(Span::styled(format!(" {row}"), bold(c)))),
        );
        (lines, width_of(&small) + 2)
    } else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::from(Span::styled(" RADIO//GARDEN", bold(GREEN))),
            ]),
            area,
        );
        return;
    };

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(art_width), Constraint::Min(0)])
        .split(area);
    frame.render_widget(Paragraph::new(lines), cols[0]);
    if cols[1].width < BANNER_INFO_WIDTH {
        return;
    }

    let link = if app.loading.is_some() {
        Span::styled(format!("{} SYNC", spinner(app)), bold(AMBER))
    } else if matches!(app.notice, Some(Notice::Error(_))) {
        Span::styled("● DEGRADED", bold(RED))
    } else {
        Span::styled("● ONLINE", bold(GREEN))
    };
    let info = vec![
        Line::raw(""),
        Line::from(vec![
            Span::styled(" [sys] ", s(DIM)),
            Span::styled(concat!("radio_tui v", env!("CARGO_PKG_VERSION")), s(MID)),
        ]),
        Line::from(vec![
            Span::styled(" [net] ", s(DIM)),
            Span::styled("uplink radio.garden ", s(MID)),
            link,
        ]),
        Line::from(vec![
            Span::styled(" [idx] ", s(DIM)),
            Span::styled(format!("{} nodes indexed", app.places.len()), s(MID)),
        ]),
        Line::from(vec![
            Span::styled(" [aud] ", s(DIM)),
            Span::styled(
                format!("gain {:>3}% ", (app.volume * 100.0).round()),
                s(MID),
            ),
            Span::styled(volume_bar(app.volume, 10), s(DIM)),
        ]),
    ];
    frame.render_widget(Paragraph::new(info), cols[1]);
}

fn render_breadcrumb(frame: &mut Frame, app: &App, area: Rect) {
    let place = |p: Option<&crate::app::PlaceSummary>| {
        p.map(|p| p.title.to_uppercase()).unwrap_or_default()
    };
    let trail: Vec<String> = match app.screen {
        Screen::Places => vec!["PLACES".into()],
        Screen::Stations => vec!["PLACES".into(), place(app.current_place.as_ref())],
        Screen::Search => vec!["SEARCH".into()],
        Screen::NowPlaying => vec![
            "TUNER".into(),
            app.now_playing
                .as_ref()
                .map(|s| s.title.to_uppercase())
                .unwrap_or_default(),
        ],
    };

    let mut left = vec![Span::styled(" ◉ ", bold(GREEN))];
    let last = trail.len().saturating_sub(1);
    for (i, part) in trail.into_iter().enumerate() {
        if i > 0 {
            left.push(Span::styled(" ▸ ", s(DARK)));
        }
        left.push(Span::styled(
            part,
            if i == last { bold(GREEN) } else { s(DIM) },
        ));
    }

    let right = match &app.loading {
        Some(what) => Span::styled(format!("{} {}... ", spinner(app), what), bold(AMBER)),
        None => Span::raw(""),
    };
    let right_width = right.width() as u16;

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_width)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().bg(BG_ALT)),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(right))
            .alignment(Alignment::Right)
            .style(Style::default().bg(BG_ALT)),
        cols[1],
    );
}

// ---------------------------------------------------------------------------
// world map
// ---------------------------------------------------------------------------

/// Lists get a world map beside them when the terminal is at least this wide.
const MAP_BESIDE_LIST_WIDTH: u16 = 120;

fn lat_lon_label((lon, lat): (f64, f64)) -> String {
    format!(
        "{:.2}°{} {:.2}°{}",
        lat.abs(),
        if lat >= 0.0 { 'N' } else { 'S' },
        lon.abs(),
        if lon >= 0.0 { 'E' } else { 'W' }
    )
}

/// Braille world map with every known place as a dot and a blinking marker
/// at `target` (lon, lat).
fn render_world(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    target: Option<(f64, f64)>,
    label: Option<&str>,
) {
    let title = match target {
        Some(t) => format!("WORLD // {}", lat_lon_label(t)),
        None => "WORLD".to_string(),
    };
    let pulse = blink(app);
    let canvas = Canvas::default()
        .block(panel(title, false))
        .background_color(BG)
        .marker(Marker::Braille)
        .x_bounds([-180.0, 180.0])
        .y_bounds([-90.0, 90.0])
        .paint(|ctx| {
            ctx.draw(&Map {
                color: DARK,
                resolution: MapResolution::High,
            });
            ctx.layer();
            ctx.draw(&Points {
                coords: &app.place_points,
                color: DIM,
            });
            if let Some((lon, lat)) = target {
                ctx.layer();
                let marker = if pulse { "◉" } else { "○" };
                ctx.print(lon, lat, Span::styled(marker, bold(AMBER)));
                if let Some(label) = label {
                    // Put the label on whichever side has more room.
                    let x = if lon < 60.0 {
                        lon + 6.0
                    } else {
                        lon - 6.0 - label.chars().count() as f64 * 4.0
                    };
                    ctx.print(x, lat, Span::styled(label.to_string(), bold(WHITE)));
                }
            }
        });
    frame.render_widget(canvas, area);
}

/// Split `area` into (list, map) when there's room for a map beside the list.
fn with_map(area: Rect) -> (Rect, Option<Rect>) {
    if area.width < MAP_BESIDE_LIST_WIDTH {
        return (area, None);
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .split(area);
    (cols[0], Some(cols[1]))
}

// ---------------------------------------------------------------------------
// lists
// ---------------------------------------------------------------------------

struct ListView<'a> {
    title: String,
    focused: bool,
    header: Vec<&'a str>,
    widths: Vec<Constraint>,
    rows: Vec<Row<'a>>,
    state: &'a ListState,
    empty: Text<'a>,
}

/// Render a windowed table: only rows in view are built by the caller.
fn render_list(frame: &mut Frame, area: Rect, view: ListView<'_>) {
    let block = panel(view.title, view.focused);
    if view.state.total == 0 {
        frame.render_widget(
            Paragraph::new(view.empty)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: true })
                .block(block),
            area,
        );
        return;
    }

    let header = Row::new(
        view.header
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, bold(DIM)))),
    )
    .style(Style::default().bg(BG_ALT));

    let table = Table::new(view.rows, view.widths)
        .header(header)
        .block(block)
        .column_spacing(1)
        .row_highlight_style(
            Style::default()
                .bg(GREEN)
                .fg(BG)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state =
        TableState::default().with_selected(Some(view.state.selected - view.state.offset));
    frame.render_stateful_widget(table, area, &mut state);

    if view.state.total > view.state.page {
        let mut sb = ScrollbarState::new(view.state.total.saturating_sub(1))
            .position(view.state.selected)
            .viewport_content_length(view.state.page);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("▲"))
                .end_symbol(Some("▼"))
                .track_symbol(Some("┊"))
                .thumb_symbol("█")
                .style(s(DIM)),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut sb,
        );
    }
}

fn visible(state: &ListState) -> std::ops::Range<usize> {
    let end = (state.offset + state.page).min(state.total);
    state.offset.min(end)..end
}

fn idx_cell<'a>(i: usize) -> Cell<'a> {
    Cell::from(Span::styled(format!("{:04x}", i), s(DARK)))
}

fn loading_text<'a>(app: &App, what: &str) -> Text<'a> {
    Text::from(vec![
        Line::raw(""),
        Line::from(Span::styled(
            format!("{} {what}", spinner(app)),
            bold(AMBER),
        )),
    ])
}

fn render_places(frame: &mut Frame, app: &App, area: Rect) {
    let (area, map) = with_map(area);
    if let Some(map) = map {
        let label = app
            .places
            .get(app.places_state.selected)
            .map(|p| p.title.as_str());
        render_world(frame, app, map, app.selected_place_coords(), label);
    }
    let st = &app.places_state;
    let rows = app.places[visible(st)]
        .iter()
        .enumerate()
        .map(|(rel, p)| {
            Row::new(vec![
                idx_cell(st.offset + rel),
                Cell::from(Span::styled(p.title.as_str(), s(WHITE))),
                Cell::from(Span::styled(p.country.as_str(), s(DIM))),
                Cell::from(Line::from(vec![
                    Span::styled(size_bar(p.size), s(MID)),
                    Span::styled(format!(" {:>4}", p.size), s(AMBER)),
                ])),
            ])
        })
        .collect();

    let empty = if app.loading.is_some() {
        loading_text(app, "downloading place index from radio.garden")
    } else {
        Text::from(Span::styled(
            "no places. press q and check your link.",
            s(RED),
        ))
    };

    render_list(
        frame,
        area,
        ListView {
            title: format!("PLACES {}/{}", st.selected + 1, st.total),
            focused: true,
            header: vec!["  ID", "LOCATION", "COUNTRY", "STATIONS"],
            widths: vec![
                Constraint::Length(4),
                Constraint::Percentage(45),
                Constraint::Percentage(35),
                Constraint::Length(11),
            ],
            rows,
            state: st,
            empty,
        },
    );
}

fn render_stations(frame: &mut Frame, app: &App, area: Rect) {
    let (area, map) = with_map(area);
    if let Some(map) = map {
        let place = app.current_place.as_ref();
        let coords = place.and_then(|p| app.place_coords(&p.id));
        render_world(frame, app, map, coords, place.map(|p| p.title.as_str()));
    }
    let st = &app.channels_state;
    let playing_id = app
        .now_playing
        .as_ref()
        .filter(|_| app.playback.is_active())
        .map(|c| c.channel_id.as_str());

    let rows = app.channels[visible(st)]
        .iter()
        .enumerate()
        .map(|(rel, c)| {
            let live = playing_id == Some(c.channel_id.as_str());
            Row::new(vec![
                idx_cell(st.offset + rel),
                Cell::from(Span::styled(if live { "♫" } else { " " }, bold(GREEN))),
                Cell::from(Span::styled(
                    c.title.as_str(),
                    if live { bold(GREEN) } else { s(WHITE) },
                )),
                Cell::from(Span::styled(c.location(), s(DIM))),
            ])
        })
        .collect();

    let place = app
        .current_place
        .as_ref()
        .map(|p| format!("{} // {}", p.title, p.subtitle))
        .unwrap_or_default();

    let empty = if app.loading.is_some() {
        loading_text(app, "scanning frequencies")
    } else {
        Text::from(Span::styled("no stations found here", s(DIM)))
    };

    render_list(
        frame,
        area,
        ListView {
            title: format!("{} [{}]", place.to_uppercase(), st.total),
            focused: true,
            header: vec!["  ID", "", "STATION", "LOCATION"],
            widths: vec![
                Constraint::Length(4),
                Constraint::Length(1),
                Constraint::Percentage(55),
                Constraint::Percentage(40),
            ],
            rows,
            state: st,
            empty,
        },
    );
}

fn render_search(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(SEARCH_INPUT_ROWS), Constraint::Min(0)])
        .split(area);

    let typing = app.search_focus == SearchFocus::Input;
    let mut input = vec![
        Span::styled(" grep -i ", s(DIM)),
        Span::styled("\"", s(DIM)),
        Span::styled(app.search_input.as_str(), bold(GREEN)),
    ];
    if typing && blink(app) {
        input.push(Span::styled("█", s(GREEN)));
    }
    input.push(Span::styled("\"", s(DIM)));
    if typing {
        input.push(Span::styled("   ⏎ query radio.garden · ↓ select", s(DARK)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(input)).block(panel("QUERY", typing)),
        chunks[0],
    );

    let st = &app.search_state;
    let rows = app.search_results[visible(st)]
        .iter()
        .enumerate()
        .map(|(rel, item)| {
            let (tag, tag_style, title, sub) = match item {
                SearchItem::Place(p) => ("PLACE", bold(CYAN), p.title.clone(), p.subtitle.clone()),
                SearchItem::Station(c) => ("RADIO", bold(GREEN), c.title.clone(), c.location()),
            };
            Row::new(vec![
                idx_cell(st.offset + rel),
                Cell::from(Span::styled(format!("[{tag}]"), tag_style)),
                Cell::from(Span::styled(title, s(WHITE))),
                Cell::from(Span::styled(sub, s(DIM))),
            ])
        })
        .collect();

    let source = if app.search_is_remote {
        "remote://radio.garden"
    } else {
        "local://places"
    };
    let empty = if app.loading.is_some() {
        loading_text(app, "querying radio.garden")
    } else if app.search_input.is_empty() {
        Text::from(vec![
            Line::raw(""),
            Line::from(Span::styled("type to grep the local place index", s(DIM))),
            Line::from(Span::styled(
                "press enter to search stations + places on radio.garden",
                s(DARK),
            )),
        ])
    } else {
        Text::from(Span::styled("0 matches", s(DIM)))
    };

    render_list(
        frame,
        chunks[1],
        ListView {
            title: format!("MATCHES {} <- {source}", st.total),
            focused: !typing,
            header: vec!["  ID", "TYPE", "NAME", "WHERE"],
            widths: vec![
                Constraint::Length(4),
                Constraint::Length(7),
                Constraint::Percentage(50),
                Constraint::Percentage(40),
            ],
            rows,
            state: st,
            empty,
        },
    );
}

// ---------------------------------------------------------------------------
// now playing
// ---------------------------------------------------------------------------

fn render_now_playing(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(65), Constraint::Min(4)])
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[0]);

    render_signal_info(frame, app, top[0]);
    let label = app.now_playing.as_ref().map(|s| s.title.as_str());
    render_world(frame, app, top[1], app.now_playing_coords(), label);
    render_spectrum(frame, app, rows[1]);
}

fn kv<'a>(key: &'a str, value: Span<'a>) -> Line<'a> {
    Line::from(vec![
        Span::styled(" > ", s(DIM)),
        Span::styled(format!("{key:<9}"), s(MID)),
        Span::styled(": ", s(DARK)),
        value,
    ])
}

fn buffer_span(app: &App, width: usize) -> Span<'static> {
    match app.buffer {
        Some(b) if b.capacity_seconds > 0.0 => {
            let fill = (b.seconds / b.capacity_seconds).clamp(0.0, 1.0);
            let color = if fill < 0.2 { AMBER } else { MID };
            Span::styled(
                format!(
                    "{} {:>4.1}s / {:.0}s @ {} kbps",
                    volume_bar(fill, width),
                    b.seconds,
                    b.capacity_seconds,
                    b.kbps
                ),
                s(color),
            )
        }
        _ => Span::styled("—", s(DARK)),
    }
}

fn render_signal_info(frame: &mut Frame, app: &App, area: Rect) {
    let Some(station) = &app.now_playing else {
        frame.render_widget(
            Paragraph::new("no carrier").block(panel("SIGNAL", true)),
            area,
        );
        return;
    };
    let (tag, tag_style) = status_tag(app);
    let status_style = match app.playback {
        Playback::Error(_) => s(RED),
        Playback::Playing => s(GREEN),
        _ => s(AMBER),
    };
    let lines = vec![
        Line::from(Span::styled(
            format!(" {}", station.title.to_uppercase()),
            bold(GREEN),
        )),
        Line::raw(""),
        kv("location", Span::styled(station.location(), s(WHITE))),
        kv(
            "status",
            Span::styled(
                format!("{tag} {}", playback_label(app)),
                tag_style.patch(status_style),
            ),
        ),
        kv("uptime", Span::styled(hms(app.elapsed_secs()), s(AMBER))),
        kv(
            "gain",
            Span::styled(
                format!(
                    "{} {:>3}%",
                    volume_bar(app.volume, 20),
                    (app.volume * 100.0).round()
                ),
                s(MID),
            ),
        ),
        kv(
            "endpoint",
            Span::styled(
                app.stream_url
                    .clone()
                    .unwrap_or_else(|| "resolving...".into()),
                s(DIM),
            ),
        ),
        kv("buffer", buffer_span(app, 20)),
        kv(
            "node",
            Span::styled(format!("rg://{}", station.channel_id), s(DIM)),
        ),
    ];
    frame.render_widget(Paragraph::new(lines).block(panel("SIGNAL", true)), area);
}

/// A (simulated) spectrum analyser: smooth pseudo-random bands scaled by the
/// volume, flat when nothing is playing.
fn render_spectrum(frame: &mut Frame, app: &App, area: Rect) {
    let block = panel("SPECTRUM", false);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w < 2 || h == 0 {
        return;
    }

    let live = app.playback == Playback::Playing;
    let t = app.tick as f32 * 0.35;
    let bands = w / 2;
    let levels: Vec<f32> = (0..bands)
        .map(|b| {
            if !live {
                return 0.04;
            }
            let x = b as f32 / bands.max(1) as f32;
            let wave = (t + b as f32 * 0.55).sin() * 0.5
                + (t * 1.7 + b as f32 * 1.3).sin() * 0.3
                + (t * 0.6 - b as f32 * 0.21).sin() * 0.2;
            let jitter = (hash(b as u64, app.tick / 2) % 100) as f32 / 100.0;
            let tilt = 1.0 - x * 0.55; // bass-heavy, like real music
            ((wave * 0.5 + 0.5) * 0.7 + jitter * 0.3) * tilt * (0.35 + app.volume * 0.65)
        })
        .collect();

    let lines: Vec<Line> = (0..h)
        .map(|row| {
            let from_bottom = h - 1 - row;
            let color = match from_bottom as f32 / h as f32 {
                f if f > 0.8 => RED,
                f if f > 0.6 => AMBER,
                f if f > 0.3 => GREEN,
                _ => MID,
            };
            let spans: Vec<Span> = levels
                .iter()
                .map(|lvl| {
                    let cells = lvl.clamp(0.0, 1.0) * h as f32;
                    let fill = (cells - from_bottom as f32).clamp(0.0, 1.0);
                    let ch = BARS[(fill * 8.0).round() as usize];
                    Span::styled(format!("{ch} "), s(color))
                })
                .collect();
            Line::from(spans)
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// bottom bars
// ---------------------------------------------------------------------------

fn render_player_bar(frame: &mut Frame, app: &App, area: Rect) {
    let (tag, tag_style) = status_tag(app);
    let mut spans = vec![Span::styled(format!(" {tag} "), tag_style)];

    match &app.now_playing {
        Some(station) => {
            let pulse = if app.playback == Playback::Playing {
                let frames = ["▁▃▅", "▃▅▇", "▅▇▅", "▇▅▃", "▅▃▁", "▃▁▃"];
                frames[(app.tick as usize / 2) % frames.len()]
            } else {
                "▁▁▁"
            };
            spans.push(Span::styled(format!("{pulse} "), s(GREEN)));
            spans.push(Span::styled(station.title.as_str(), bold(WHITE)));
            let loc = station.location();
            if !loc.is_empty() {
                spans.push(Span::styled(format!("  // {loc}"), s(DIM)));
            }
            spans.push(Span::styled(
                format!("  {}", playback_label(app)),
                match app.playback {
                    Playback::Error(_) => s(RED),
                    Playback::Playing => s(MID),
                    _ => s(AMBER),
                },
            ));
        }
        None => spans.push(Span::styled(
            "no carrier — pick a place, then a station",
            s(DIM),
        )),
    }

    let buf = app
        .buffer
        .map(|b| format!("buf {:>2.0}s  ", b.seconds))
        .unwrap_or_default();
    let right = Line::from(vec![
        Span::styled(buf, s(DIM)),
        Span::styled(format!("{} ", hms(app.elapsed_secs())), s(AMBER)),
        Span::styled("vol ", s(DIM)),
        Span::styled(volume_bar(app.volume, 10), s(MID)),
        Span::styled(format!(" {:>3}% ", (app.volume * 100.0).round()), s(MID)),
    ]);
    let right_width = right.width() as u16 + 1;

    let block = panel("TUNER", app.playback.is_active());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_width)])
        .split(inner);
    frame.render_widget(Paragraph::new(Line::from(spans)), cols[0]);
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), cols[1]);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = match app.screen {
        Screen::Places => &[
            ("⏎", "open"),
            ("/", "grep"),
            ("r", "random"),
            ("spc", "play/stop"),
            ("n", "tuner"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Screen::Stations => &[
            ("⏎", "tune"),
            ("r", "random"),
            ("esc", "back"),
            ("spc", "play/stop"),
            ("+/-", "gain"),
            ("?", "help"),
        ],
        Screen::Search if app.search_focus == SearchFocus::Input => &[
            ("⏎", "query"),
            ("↓", "results"),
            ("esc", "back"),
            ("^c", "quit"),
        ],
        Screen::Search => &[
            ("⏎", "open"),
            ("/", "edit"),
            ("esc", "query"),
            ("spc", "play/stop"),
        ],
        Screen::NowPlaying => &[
            ("spc", "play/stop"),
            ("+/-", "gain"),
            ("r", "random"),
            ("esc", "back"),
            ("q", "quit"),
        ],
    };

    let mut spans = vec![Span::raw(" ")];
    for (k, d) in keys {
        spans.push(Span::styled(format!("[{k}]"), bold(AMBER)));
        spans.push(Span::styled(format!(" {d}  "), s(DIM)));
    }

    let notice = match &app.notice {
        Some(Notice::Error(m)) => Span::styled(format!(" [ERR] {m} "), bold(RED)),
        Some(Notice::Info(m)) => Span::styled(format!(" [i] {m} "), s(CYAN)),
        None => Span::raw(""),
    };
    let nw = (notice.width() as u16).min(area.width / 2);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(nw)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(BG_ALT)),
        cols[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(notice))
            .alignment(Alignment::Right)
            .style(Style::default().bg(BG_ALT)),
        cols[1],
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let w = 58.min(area.width.saturating_sub(4));
    let h = 22.min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let entry = |k: &'static str, d: &'static str| {
        Line::from(vec![
            Span::styled(format!("   {k:<14}"), bold(AMBER)),
            Span::styled(d, s(WHITE)),
        ])
    };
    let section = |t: &'static str| Line::from(Span::styled(format!(" {t}"), bold(GREEN)));
    let lines = vec![
        section("NAVIGATION"),
        entry("j/k  ↑/↓", "move"),
        entry("PgUp/PgDn", "page"),
        entry("g/G Home/End", "first / last"),
        entry("⏎  l  →", "open place / tune station"),
        entry("esc h ←", "back"),
        Line::raw(""),
        section("PLAYBACK"),
        entry("space  p", "stop / resume"),
        entry("+  -", "gain up / down"),
        entry("r", "random place / station"),
        entry("n", "jump to tuner"),
        Line::raw(""),
        section("SEARCH"),
        entry("/", "grep places (live, local)"),
        entry("⏎ (in query)", "search radio.garden"),
        entry("↓ / tab", "jump into results"),
        Line::raw(""),
        Line::from(Span::styled("        press any key to close", s(DIM))),
    ];
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            panel("MAN RADIO(1)", true)
                .border_set(border::DOUBLE)
                .style(Style::default().bg(BG_ALT)),
        ),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ChannelListItem, Place};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn banner_rows_line_up() {
        let w = width_of(&SHADOW_LOGO);
        assert!(SHADOW_LOGO.iter().all(|r| r.chars().count() as u16 == w));
        for glyph in SMALL_GLYPHS {
            let gw = glyph.1[0].chars().count();
            assert!(
                glyph.1.iter().all(|r| r.chars().count() == gw),
                "{:?}",
                glyph.0
            );
        }
        assert!(w > width_of(&assemble(SMALL_GLYPHS)));
    }

    #[test]
    fn list_rows_accounts_for_chrome() {
        assert!(list_rows(40, Screen::Places) > list_rows(40, Screen::Search));
        assert_eq!(list_rows(3, Screen::Places), 1, "never zero");
    }

    fn draw(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    fn sample_app() -> App {
        let mut app = App::new();
        app.set_places(
            (0..50)
                .map(|i| Place {
                    id: format!("p{i}"),
                    title: format!("Place {i}"),
                    country: "Nowhere".into(),
                    geo: vec![0.0, 0.0],
                    size: i,
                    boost: false,
                    url: String::new(),
                })
                .collect(),
        );
        app
    }

    #[test]
    fn renders_every_screen_without_panicking() {
        let mut app = sample_app();
        for (w, h) in [(120, 40), (80, 24), (30, 10), (10, 5)] {
            app.set_list_height(list_rows(h, app.screen));
            for screen in [
                Screen::Places,
                Screen::Stations,
                Screen::Search,
                Screen::NowPlaying,
            ] {
                app.screen = screen;
                draw(&app, w, h);
            }
        }
    }

    #[test]
    fn places_screen_shows_places_and_prompt() {
        let mut app = sample_app();
        app.set_list_height(list_rows(40, Screen::Places));
        let out = draw(&app, 120, 40);
        assert!(out.contains("Place 49"), "busiest place first");
        assert!(out.contains("PLACES"));
    }

    #[test]
    fn now_playing_shows_station() {
        let mut app = sample_app();
        app.now_playing = Some(ChannelListItem {
            title: "KUTX".into(),
            channel_id: "vbFsCngB".into(),
            place: None,
            country: None,
            website: None,
        });
        app.playback = Playback::Playing;
        app.screen = Screen::NowPlaying;
        app.tick = 17;
        let out = draw(&app, 120, 40);
        assert!(out.contains("KUTX"));
        assert!(out.contains("LIVE"));
    }

    #[test]
    fn help_overlay_renders() {
        let mut app = sample_app();
        app.show_help = true;
        assert!(draw(&app, 100, 30).contains("MAN RADIO(1)"));
    }
}
