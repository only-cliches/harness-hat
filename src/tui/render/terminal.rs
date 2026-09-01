use super::*;

pub(crate) fn render_terminal(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    session_idx: usize,
    dimmed: bool,
    fullscreen: bool,
) {
    let (term, container_id, tab_label, session_exited, terminal_detached, shell_command) =
        match app.sessions.get(session_idx) {
            Some(s) => (
                std::sync::Arc::clone(&s.term),
                s.container_id.clone(),
                s.tab_label(),
                s.is_exited(),
                s.is_terminal_detached(),
                s.shell_in_hint(),
            ),
            None => return,
        };

    let focused = app.focus == Focus::Terminal;
    let in_scroll_mode = focused && app.scroll_mode;
    let border_style = if in_scroll_mode {
        Style::default().fg(Color::Yellow)
    } else if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let short_id = if container_id.len() > 12 {
        &container_id[..12]
    } else {
        &container_id
    };
    let tab_title = if in_scroll_mode {
        format!(" {} [{}] -- SCROLL -- ", tab_label, short_id)
    } else {
        format!(" {} [{}] ", tab_label, short_id)
    };
    let title_style = if in_scroll_mode {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    };
    let content_area = if fullscreen {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(area);
        render_terminal_fullscreen_header(
            frame,
            split[0],
            tab_title.as_str(),
            title_style,
            terminal_fullscreen_hint(true),
        );
        split[1]
    } else {
        area
    };

    if content_area.height == 0 || content_area.width == 0 {
        return;
    }

    let block = if fullscreen {
        Block::default()
    } else {
        Block::default()
            .title(tab_title.as_str())
            .title_style(title_style)
            .borders(Borders::ALL)
            .border_style(border_style)
    };

    let inner = if fullscreen {
        content_area
    } else {
        block.inner(content_area)
    };
    if focused {
        app.terminal_selection_area = Some(inner);
    }
    frame.render_widget(block, content_area);
    if !fullscreen {
        render_terminal_border_hint(frame, content_area, terminal_fullscreen_hint(false));
    }

    if let Some(session) = app.sessions.get_mut(session_idx)
        && !terminal_detached
    {
        let _ = session.resize(inner.height, inner.width);
    }

    if terminal_detached {
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "Terminal connection closed; container is still running.",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("Reconnect from a host terminal: {shell_command}"),
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        return;
    }

    let mut term = term.lock();
    if !session_exited && !term_has_content(&term) {
        let spinner = loading_spinner_frame();
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("{spinner} Starting container..."),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Waiting for terminal output",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
        return;
    }

    render_term_buffer(
        frame,
        inner,
        &mut *term,
        dimmed,
        focused,
        app.scroll_mode,
        app.terminal_scroll,
    );
}

pub(crate) fn render_session_detail(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    session_idx: usize,
    dimmed: bool,
) {
    let Some(session) = app.sessions.get(session_idx) else {
        render_idle(frame, area);
        return;
    };

    let docker_name = session.docker_name.clone();
    let container_id = session.container_id.clone();
    let project = session.workspace_name.clone();
    let container_name = session.container_name.clone();
    let mount_target = session.mount_target.clone();
    let shell_commands = session.shell_commands();
    let exited = session.is_exited();
    let terminal_detached = session.is_terminal_detached();
    let launched_secs = session.launched_at.elapsed().as_secs();
    let usage = if exited {
        None
    } else {
        app.container_usage_for_session(session_idx)
    };

    let cfg = app.config.get();
    let workspace_path = cfg
        .workspaces
        .iter()
        .find(|workspace| workspace.name == project)
        .map(|workspace| crate::fs_util::display_host_path(&workspace.canonical_path))
        .unwrap_or_else(|| "<unknown>".to_string());
    let templates = app.workspace_templates_for_name(&project);
    let template = templates
        .iter()
        .find(|template| template.name == container_name);
    let image = template
        .map(|template| template.image.as_str())
        .unwrap_or("<unknown>");
    let extra_mounts = template
        .map(|template| template.mounts.clone())
        .unwrap_or_default();

    let tone = |c| maybe_dim(c, dimmed);
    let focused = app.focus == Focus::Terminal && !dimmed;
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let block = Block::default()
        .title(" Session ")
        .title_style(
            Style::default()
                .fg(tone(Color::Cyan))
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tone(border)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let status = if exited {
        "stopped"
    } else if terminal_detached {
        "running (terminal detached)"
    } else {
        "running"
    };
    let status_color = if exited {
        Color::DarkGray
    } else if terminal_detached {
        Color::Yellow
    } else {
        Color::Green
    };
    let short_id = short_container_id(&container_id);
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Workspace : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(project.clone(), Style::default().fg(tone(Color::White))),
        ]),
        Line::from(vec![
            Span::styled("  Template  : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(
                container_name.clone(),
                Style::default().fg(tone(Color::White)),
            ),
            Span::styled("  image ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(image.to_string(), Style::default().fg(tone(Color::White))),
        ]),
        Line::from(vec![
            Span::styled("  Container : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(docker_name.clone(), Style::default().fg(tone(Color::White))),
            Span::styled(
                format!("  {short_id}"),
                Style::default().fg(tone(Color::DarkGray)),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Status    : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(status, Style::default().fg(tone(status_color))),
            Span::styled(
                format!("  uptime {}s", launched_secs),
                Style::default().fg(tone(Color::DarkGray)),
            ),
        ]),
    ];

    if let Some(usage) = usage {
        lines.push(Line::from(vec![
            Span::styled("  Usage     : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(
                format!("CPU {}  Memory {}", usage.cpu_percent, usage.memory_usage),
                Style::default().fg(tone(Color::White)),
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Usage     : ", Style::default().fg(tone(Color::DarkGray))),
            Span::styled(
                if exited {
                    "container stopped"
                } else {
                    "pending"
                },
                Style::default().fg(tone(Color::DarkGray)),
            ),
        ]));
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "  Shell",
            Style::default()
                .fg(tone(Color::Cyan))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  hat sh commands for this session:",
            Style::default().fg(tone(Color::DarkGray)),
        )),
    ]);
    lines.extend(shell_commands.into_iter().map(|command| {
        Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(command, Style::default().fg(tone(Color::White))),
        ])
    }));
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "  Mounts",
            Style::default()
                .fg(tone(Color::Cyan))
                .add_modifier(Modifier::BOLD),
        )),
    ]);

    // Collect all mount rows before rendering so we can align columns.
    let mut mount_rows: Vec<(String, Color, String, &str, String, Option<&str>)> = vec![(
        "rw".to_string(),
        Color::Green,
        workspace_path,
        "->",
        mount_target,
        None,
    )];
    for mount in &extra_mounts {
        // A seeded mount isn't a live bind: a private copy is taken at launch and
        // the container owns it, so don't render it like the rw/ro binds above.
        let seeded = mount.is_seeded();
        let (label, label_color, arrow) = if seeded {
            ("seed".to_string(), Color::Yellow, "~>")
        } else {
            (
                crate::container::mount_mode_arg(&mount.mode).to_string(),
                Color::Green,
                "->",
            )
        };
        mount_rows.push((
            label,
            label_color,
            crate::fs_util::display_host_path(&mount.host),
            arrow,
            crate::config::container_path_string(&mount.container),
            if seeded {
                Some("(per-session copy)")
            } else {
                None
            },
        ));
    }

    let label_w = mount_rows.iter().map(|(l, ..)| l.len()).max().unwrap_or(2);
    let host_w = mount_rows
        .iter()
        .map(|(_, _, h, ..)| h.len())
        .max()
        .unwrap_or(4);

    let mode_hdr = format!("{:<label_w$}", "Mode", label_w = label_w);
    let host_hdr = format!("{:<host_w$}", "Host", host_w = host_w);
    lines.push(Line::from(Span::styled(
        format!("  {mode_hdr}  {host_hdr}      Container"),
        Style::default().fg(tone(Color::DarkGray)),
    )));

    for (label, label_color, host, arrow, container, note) in &mount_rows {
        let label_col = format!("{:<label_w$}", label, label_w = label_w);
        let host_col = format!("{:<host_w$}", host, host_w = host_w);
        let mut spans = vec![
            Span::styled(
                format!("  {label_col}"),
                Style::default().fg(tone(*label_color)),
            ),
            Span::styled(
                format!("  {host_col}"),
                Style::default().fg(tone(Color::White)),
            ),
            Span::styled(
                format!("  {arrow}  "),
                Style::default().fg(tone(Color::DarkGray)),
            ),
            Span::styled(container.clone(), Style::default().fg(tone(Color::White))),
        ];
        if let Some(note) = note {
            spans.push(Span::styled(
                format!("  {note}"),
                Style::default().fg(tone(Color::DarkGray)),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            "  Actions",
            Style::default()
                .fg(tone(Color::Cyan))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "  [k] stop container  [x] kill network connections  [Esc/^B] sidebar",
            Style::default().fg(tone(Color::DarkGray)),
        )),
    ]);

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(crate) fn terminal_fullscreen_hint(fullscreen: bool) -> &'static str {
    if fullscreen {
        " Ctrl+G to exit fullscreen "
    } else {
        " Ctrl+G for full screen "
    }
}

fn render_terminal_border_hint(frame: &mut Frame, area: Rect, hint: &str) {
    let hint_width = hint.chars().count() as u16;
    if area.height == 0 || area.width <= hint_width + 2 {
        return;
    }

    let hint_area = Rect {
        x: area.x + area.width - hint_width - 1,
        y: area.y,
        width: hint_width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Span::styled(
            hint.to_string(),
            Style::default().fg(Color::DarkGray),
        )),
        hint_area,
    );
}

pub(crate) fn render_term_buffer<T: alacritty_terminal::event::EventListener>(
    frame: &mut Frame,
    inner: Rect,
    term: &mut alacritty_terminal::term::Term<T>,
    dimmed: bool,
    focused: bool,
    scroll_mode: bool,
    terminal_scroll: usize,
) {
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let desired_offset = if scroll_mode { terminal_scroll } else { 0 };
    let max_scrollback = term.history_size();
    let desired_offset = desired_offset.min(max_scrollback);
    let current_offset = term.grid().display_offset();
    if desired_offset != current_offset {
        let delta = desired_offset as i32 - current_offset as i32;
        term.scroll_display(Scroll::Delta(delta));
    }
    let actual_scroll = term.grid().display_offset();

    let rows = inner.height as usize;
    let cols = inner.width as usize;
    let mut content = term.renderable_content();

    let default_fg = resolve_ansi_color(AnsiColor::Named(NamedColor::Foreground), content.colors);
    let default_bg = resolve_ansi_color(AnsiColor::Named(NamedColor::Background), content.colors);
    let mut default_style = Style::default().fg(default_fg).bg(default_bg);
    if dimmed {
        default_style = attenuate_style(default_style);
    }

    let cursor_point = content.cursor.point;
    let show_cursor = focused
        && !dimmed
        && actual_scroll == 0
        && content
            .mode
            .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR);

    #[derive(Clone)]
    struct CellOut {
        ch: char,
        style: Style,
        skip: bool,
    }

    let mut grid: Vec<CellOut> = vec![
        CellOut {
            ch: ' ',
            style: default_style,
            skip: false,
        };
        rows * cols
    ];

    for indexed in content.display_iter.by_ref() {
        let Some(vp) =
            alacritty_terminal::term::point_to_viewport(content.display_offset, indexed.point)
        else {
            continue;
        };
        let row = vp.line;
        let col = vp.column.0;
        if col >= cols {
            continue;
        }
        let row_offset = term.screen_lines().saturating_sub(rows);
        if row < row_offset || row >= row_offset + rows {
            continue;
        }
        let rr = row - row_offset;
        let idx = rr * cols + col;

        let cell = indexed.cell;
        let mut ch = cell.c;
        let skip = cell.flags.contains(TermFlags::WIDE_CHAR_SPACER);
        if cell.flags.contains(TermFlags::HIDDEN) {
            ch = ' ';
        }

        let mut fg_src = cell.fg;
        let bg_src = cell.bg;
        let missing_default_palette = content.colors[NamedColor::Foreground].is_none()
            && content.colors[NamedColor::Background].is_none();
        if missing_default_palette
            && matches!(fg_src, AnsiColor::Spec(Rgb { r: 0, g: 0, b: 0 }))
            && matches!(bg_src, AnsiColor::Named(NamedColor::Background))
            && cell.flags.contains(TermFlags::BOLD)
        {
            fg_src = AnsiColor::Named(NamedColor::Foreground);
        }
        if cell.flags.contains(TermFlags::BOLD)
            && !cell.flags.contains(TermFlags::DIM)
            && !cell.flags.contains(TermFlags::DIM_BOLD)
        {
            fg_src = brighten_bold_ansi_color(fg_src);
        }

        let mut fg = resolve_ansi_color(fg_src, content.colors);
        let mut bg = resolve_ansi_color(bg_src, content.colors);
        if cell.flags.contains(TermFlags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        let mut style = Style::default().fg(fg).bg(bg);
        if cell.flags.contains(TermFlags::BOLD) {
            style = style.add_modifier(Modifier::BOLD);
        }
        if cell.flags.contains(TermFlags::ITALIC) {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if cell.flags.contains(TermFlags::ALL_UNDERLINES) {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if cell.flags.contains(TermFlags::STRIKEOUT) {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        if cell.flags.contains(TermFlags::DIM) || cell.flags.contains(TermFlags::DIM_BOLD) {
            style = attenuate_style(style.add_modifier(Modifier::DIM));
        }
        if content
            .selection
            .as_ref()
            .is_some_and(|selection| selection.contains(indexed.point))
        {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if dimmed {
            style = attenuate_style(style);
        }

        if show_cursor && indexed.point == cursor_point && rr < rows && col < cols {
            style = style.add_modifier(Modifier::REVERSED);
        }

        grid[idx] = CellOut { ch, style, skip };
    }

    let mut rendered: Vec<Line> = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        let mut cur_style: Option<Style> = None;
        let mut cur_text = String::new();
        for c in 0..cols {
            let cell = &grid[r * cols + c];
            if cell.skip {
                continue;
            }
            if cur_style == Some(cell.style) {
                cur_text.push(cell.ch);
            } else {
                if let Some(style) = cur_style.take() {
                    spans.push(Span::styled(std::mem::take(&mut cur_text), style));
                }
                cur_style = Some(cell.style);
                cur_text.push(cell.ch);
            }
        }
        if let Some(style) = cur_style.take() {
            spans.push(Span::styled(cur_text, style));
        }
        rendered.push(Line::from(spans));
    }

    frame.render_widget(Paragraph::new(rendered), inner);

    if scroll_mode && max_scrollback > 0 {
        render_scrollbar(frame, inner, max_scrollback, actual_scroll, true);
    }
}

pub(crate) fn resolve_ansi_color(
    color: AnsiColor,
    colors: &alacritty_terminal::term::color::Colors,
) -> Color {
    match color {
        AnsiColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        AnsiColor::Named(named) => {
            if let Some(rgb) = colors[named] {
                if matches!(named, NamedColor::Foreground | NamedColor::BrightForeground) {
                    let fg_is_blackish = rgb.r <= 0x10 && rgb.g <= 0x10 && rgb.b <= 0x10;
                    let bg_is_blackish = colors[NamedColor::Background]
                        .map(|bg| bg.r <= 0x10 && bg.g <= 0x10 && bg.b <= 0x10)
                        .unwrap_or(true);
                    if fg_is_blackish && bg_is_blackish {
                        return Color::Rgb(0xff, 0xff, 0xff);
                    }
                }
                return Color::Rgb(rgb.r, rgb.g, rgb.b);
            }
            match named {
                NamedColor::Foreground => Color::White,
                NamedColor::Background => Color::Black,
                NamedColor::BrightForeground => Color::White,
                NamedColor::DimForeground => Color::Gray,
                _ => Color::Reset,
            }
        }
        AnsiColor::Indexed(idx) => {
            if let Some(rgb) = colors[idx as usize] {
                return Color::Rgb(rgb.r, rgb.g, rgb.b);
            }
            let (r, g, b) = crate::ansi::xterm_256_to_rgb(idx);
            Color::Rgb(r, g, b)
        }
    }
}

pub(crate) fn brighten_bold_ansi_color(color: AnsiColor) -> AnsiColor {
    match color {
        AnsiColor::Named(named) => AnsiColor::Named(match named {
            NamedColor::Black => NamedColor::BrightBlack,
            NamedColor::Red => NamedColor::BrightRed,
            NamedColor::Green => NamedColor::BrightGreen,
            NamedColor::Yellow => NamedColor::BrightYellow,
            NamedColor::Blue => NamedColor::BrightBlue,
            NamedColor::Magenta => NamedColor::BrightMagenta,
            NamedColor::Cyan => NamedColor::BrightCyan,
            NamedColor::White => NamedColor::BrightWhite,
            other => other,
        }),
        AnsiColor::Indexed(idx) if idx <= 7 => AnsiColor::Indexed(idx + 8),
        other => other,
    }
}

pub(crate) fn term_has_content<T: alacritty_terminal::event::EventListener>(
    term: &alacritty_terminal::term::Term<T>,
) -> bool {
    let content = term.renderable_content();
    for indexed in content.display_iter {
        let ch = indexed.cell.c;
        if !ch.is_whitespace() {
            return true;
        }
    }
    false
}

pub(crate) fn loading_spinner_frame() -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as usize)
        .unwrap_or(0);
    FRAMES[(ms / 120) % FRAMES.len()]
}

/// Attenuate a style's foreground and background colors (when set), used to
/// dim inactive panes and DIM-flagged cells.
pub(crate) fn attenuate_style(mut style: Style) -> Style {
    if let Some(fg) = style.fg {
        style = style.fg(attenuate_color(fg));
    }
    if let Some(bg) = style.bg {
        style = style.bg(attenuate_color(bg));
    }
    style
}

pub(crate) fn attenuate_color(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => Color::Rgb(scale_channel(r), scale_channel(g), scale_channel(b)),
        Color::Black => Color::Black,
        Color::Red => Color::DarkGray,
        Color::Green => Color::DarkGray,
        Color::Yellow => Color::DarkGray,
        Color::Blue => Color::DarkGray,
        Color::Magenta => Color::DarkGray,
        Color::Cyan => Color::DarkGray,
        Color::Gray => Color::DarkGray,
        Color::DarkGray => Color::DarkGray,
        Color::LightRed => Color::DarkGray,
        Color::LightGreen => Color::DarkGray,
        Color::LightYellow => Color::DarkGray,
        Color::LightBlue => Color::DarkGray,
        Color::LightMagenta => Color::DarkGray,
        Color::LightCyan => Color::DarkGray,
        Color::White => Color::Gray,
        Color::Indexed(n) => {
            if n >= 8 {
                Color::DarkGray
            } else {
                Color::Indexed(n)
            }
        }
        Color::Reset => Color::Reset,
    }
}

pub(crate) fn scale_channel(v: u8) -> u8 {
    ((v as f32) * 0.45).round() as u8
}

pub(crate) fn maybe_dim(color: Color, dimmed: bool) -> Color {
    if dimmed {
        attenuate_color(color)
    } else {
        color
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_fullscreen_hint_tracks_state() {
        assert_eq!(terminal_fullscreen_hint(false), " Ctrl+G for full screen ");
        assert_eq!(
            terminal_fullscreen_hint(true),
            " Ctrl+G to exit fullscreen "
        );
    }
}
