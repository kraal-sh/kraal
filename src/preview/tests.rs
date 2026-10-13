//! Tests of previews: loading, layouts, Markdown and images.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use image::{Rgba, RgbaImage};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

use super::markdown::{html_attr, markdown_lines};
use super::spans::line_text;
use super::{CODE_BLOCK, DIR_NAME, INLINE_CODE, Preview, PreviewKind, gutter, indicator, link_url};
use crate::text::human_size;

/// A preview read now, on this thread.
fn loaded(path: &Path) -> Preview {
    let mut preview = Preview::open(path);
    preview.apply(super::Loaded::read(path, false));
    preview
}

/// A unique scratch directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kraal-preview-{}-{name}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Rows of `area` as text, skipping the cells hidden by wide characters.
fn rows(buf: &Buffer, area: Rect) -> Vec<String> {
    (area.top()..area.bottom())
        .map(|y| {
            let mut row = String::new();
            let mut x = area.left();
            while x < area.right() {
                let symbol = buf[(x, y)].symbol();
                row.push_str(symbol);
                x += u16::try_from(symbol.width().max(1)).unwrap();
            }
            row.trim_end().to_owned()
        })
        .collect()
}

fn render(preview: &mut Preview, width: u16, height: u16) -> (Buffer, Vec<String>) {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    preview.render(area, &mut buf);
    let text = rows(&buf, area);
    (buf, text)
}

fn md(source: &str, width: usize) -> Vec<String> {
    markdown_lines(source, width).iter().map(|l| line_text(l).trim_end().to_owned()).collect()
}

fn png(dir: &TempDir, name: &str, width: u32, pixels: &[[u8; 4]]) -> PathBuf {
    let height = u32::try_from(pixels.len()).unwrap() / width;
    let image = RgbaImage::from_fn(width, height, |x, y| Rgba(pixels[(y * width + x) as usize]));
    let path = dir.0.join(name);
    image.save(&path).unwrap();
    path
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

#[test]
fn markdown_headings_are_styled_and_ruled() {
    let dir = TempDir::new("headings");
    let path = dir.write("README.md", "# Title\n\nHello *world* and **bold**\n\n## Sub\n\n### Third\n");
    let mut preview = loaded(&path);
    assert_eq!(preview.kind(), PreviewKind::Markdown);
    assert_eq!(preview.title(), "README.md");
    let (buf, text) = render(&mut preview, 30, 12);
    assert_eq!(text[0], "Title");
    assert_eq!(text[1], "━".repeat(30));
    assert_eq!(text[2], "");
    assert_eq!(text[3], "Hello world and bold");
    assert_eq!(text[5], "Sub");
    assert_eq!(text[6], "───");
    assert_eq!(text[8], "Third");
    let title = &buf[(0, 0)];
    assert!(title.modifier.contains(Modifier::BOLD));
    assert_eq!(title.fg, Color::LightMagenta);
    assert_eq!(buf[(0, 5)].fg, Color::LightCyan);
    assert!(buf[(6, 3)].modifier.contains(Modifier::ITALIC), "world is italic");
    assert!(buf[(16, 3)].modifier.contains(Modifier::BOLD), "bold is bold");
    assert!(!buf[(0, 3)].modifier.contains(Modifier::ITALIC));
}

#[test]
fn markdown_inline_code_and_strikethrough() {
    let dir = TempDir::new("inline");
    let path = dir.write("a.md", "use `foo()` not ~~bar~~");
    let mut preview = loaded(&path);
    let (buf, text) = render(&mut preview, 40, 3);
    assert_eq!(text[0], "use foo() not bar");
    assert_eq!(buf[(4, 0)].bg, INLINE_CODE.bg.unwrap());
    assert_eq!(buf[(0, 0)].bg, Color::Reset);
    assert!(buf[(14, 0)].modifier.contains(Modifier::CROSSED_OUT));
}

#[test]
fn markdown_lists_have_hanging_indents() {
    let source = "- one\n- two words that wrap around\n  - nested item\n\n1. first\n2. second\n";
    let lines = md(source, 20);
    assert_eq!(lines[0], "• one");
    assert_eq!(lines[1], "• two words that");
    assert_eq!(lines[2], "  wrap around");
    assert_eq!(lines[3], "  ◦ nested item");
    assert_eq!(lines[4], "");
    assert_eq!(lines[5], "1. first");
    assert_eq!(lines[6], "2. second");
    assert!(lines.iter().all(|l| l.width() <= 20));
}

#[test]
fn markdown_ordered_numbers_are_aligned() {
    let source: String = (1..=10).map(|i| format!("{i}. item {i}\n")).collect();
    let lines = md(&source, 40);
    assert_eq!(lines[0], " 1. item 1");
    assert_eq!(lines[9], "10. item 10");
    let wrapped = md("9. a long item that wraps\n10. b\n", 12);
    assert_eq!(wrapped[0], " 9. a long");
    assert_eq!(wrapped[1], "    item");
}

#[test]
fn markdown_task_lists() {
    let lines = md("- [x] done thing\n- [ ] todo\n", 12);
    assert_eq!(lines, ["[x] done", "    thing", "[ ] todo"]);
}

#[test]
fn markdown_code_blocks_have_label_and_background() {
    let dir = TempDir::new("code");
    let path = dir.write("c.md", "Text\n\n```rust\nfn main() {\n\tx();\n}\n```\n");
    let mut preview = loaded(&path);
    let (buf, text) = render(&mut preview, 24, 8);
    assert_eq!(text[0], "Text");
    assert_eq!(text[2], "   rust");
    assert_eq!(text[3], "   fn main() {");
    assert_eq!(text[4], "       x();");
    assert_eq!(text[5], "   }");
    assert_eq!(buf[(0, 3)].bg, Color::Reset, "indent has no background");
    assert_eq!(buf[(2, 3)].bg, CODE_BLOCK.bg.unwrap());
    assert_eq!(buf[(23, 3)].bg, CODE_BLOCK.bg.unwrap(), "background spans the width");
    assert!(buf[(3, 2)].modifier.contains(Modifier::ITALIC), "label is italic");
}

#[test]
fn markdown_long_code_lines_wrap_inside_the_box() {
    let lines = md("```\nabcdefghijklmnop\n```", 12);
    assert_eq!(lines, ["   abcdefghi", "   jklmnop"]);
}

#[test]
fn markdown_block_quotes() {
    let lines = md("> quoted text that wraps\n>\n> second\n\nafter", 16);
    assert_eq!(lines, ["│ quoted text", "│ that wraps", "│", "│ second", "", "after"]);
    let nested = md("> outer\n> > inner", 20);
    assert_eq!(nested, ["│ outer", "│", "│ │ inner"]);
}

#[test]
fn markdown_tables_align_and_truncate() {
    let source = "| Name | Qty |\n|:-----|----:|\n| apple | 3 |\n| kiwi | 12 |\n";
    let lines = md(source, 40);
    assert_eq!(lines, ["Name  │ Qty", "──────┼────", "apple │   3", "kiwi  │  12"]);
    let narrow = md(source, 10);
    assert!(narrow.iter().all(|l| l.width() <= 10), "{narrow:?}");
    assert_eq!(narrow[2], "app… │   3");
    let header = markdown_lines(source, 40);
    assert!(header[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn markdown_links_show_url_once() {
    let lines = md("[docs](https://x.io) and [docs](https://x.io) and <https://y.io>", 80);
    assert_eq!(lines, ["docs (https://x.io) and docs and https://y.io"]);
    let rendered = markdown_lines("[docs](https://x.io)", 80);
    let link = &rendered[0].spans[0];
    assert_eq!(link.content, "docs");
    assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));
    assert_eq!(rendered[0].spans[1].style, link_url());
}

#[test]
fn markdown_images_rules_and_breaks() {
    assert_eq!(md("![a cat](cat.png)", 40), ["[image: a cat]"]);
    assert_eq!(md("above\n\n---\n\nbelow", 5), ["above", "", "─────", "", "below"]);
    assert_eq!(md("one  \ntwo<br>three", 40), ["one", "two", "three"]);
    assert_eq!(md("<!-- hidden -->\n\ntext", 40), ["text"]);
}

#[test]
fn markdown_html_is_reduced_to_text() {
    let block = "<p align=\"center\">\n  <img src=\"logo.png\" alt=\"Logo &amp; name\">\n</p>\n\ntext";
    assert_eq!(md(block, 40), ["[image: Logo & name]", "", "text"]);
    let details = "<details>\n<summary>More &lt;info&gt;</summary>\n\nbody\n\n</details>\n";
    assert_eq!(md(details, 40), ["More <info>", "", "body"]);
    assert_eq!(md("Press <kbd>Ctrl</kbd> + <img src=x.png> a < b", 40), ["Press Ctrl + [image] a < b"]);
    assert_eq!(html_attr("<img ALT='single' src=x>", "alt"), Some("single"));
    assert_eq!(html_attr("<img data-alt=\"no\" alt=bare>", "alt"), Some("bare"));
    assert_eq!(html_attr("<img src=x>", "alt"), None);
}

#[test]
fn markdown_rewraps_per_width() {
    let dir = TempDir::new("wrap");
    let words = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor";
    let path = dir.write("w.md", words);
    let mut preview = loaded(&path);
    let (_, wide) = render(&mut preview, 80, 10);
    assert_eq!(wide[0], words);
    let (_, narrow) = render(&mut preview, 20, 10);
    assert!(narrow.iter().all(|l| l.width() <= 20));
    assert_eq!(narrow[0], "lorem ipsum dolor");
    assert_eq!(narrow.join(" ").split_whitespace().collect::<Vec<_>>().join(" "), words);
}

#[test]
fn markdown_respects_display_width() {
    let lines = md("中文字符测试 emoji 🎉🎉 more 漢字漢字漢字漢字", 10);
    assert!(lines.iter().all(|l| l.width() <= 10), "{lines:?}");
    assert!(lines.concat().contains("🎉🎉"));
    // A width narrower than a wide character still terminates.
    assert!(!md("漢字", 1).is_empty());
}

#[test]
fn markdown_survives_odd_input_at_tiny_widths() {
    let source = "# H\n> - [x] a\n>   1. `code`\n\n| a | b |\n|---|---|\n| 中 | x |\n\n```\n\t\x07\n```\n- \n";
    for width in 1..12 {
        for line in markdown_lines(source, width) {
            let _ = line_text(&line);
        }
    }
}

#[test]
fn image_renders_half_blocks() {
    let dir = TempDir::new("image");
    let path = png(&dir, "tiny.png", 2, &[RED, GREEN, BLUE, WHITE]);
    let mut preview = loaded(&path);
    assert_eq!(preview.kind(), PreviewKind::Image);
    assert_eq!(preview.visible_text(), "[image 2×2 png]");
    let (buf, text) = render(&mut preview, 2, 1);
    assert_eq!(text, ["▀▀"]);
    assert_eq!(buf[(0, 0)].fg, Color::Rgb(255, 0, 0));
    assert_eq!(buf[(0, 0)].bg, Color::Rgb(0, 0, 255));
    assert_eq!(buf[(1, 0)].fg, Color::Rgb(0, 255, 0));
    assert_eq!(buf[(1, 0)].bg, Color::Rgb(255, 255, 255));
}

#[test]
fn image_is_scaled_and_centered() {
    let dir = TempDir::new("center");
    let path = png(&dir, "tiny.png", 2, &[RED, GREEN, BLUE, WHITE]);
    let mut preview = loaded(&path);
    // 10×2 cells = 10×4 pixels: the square image becomes 4×4, centered.
    let (buf, text) = render(&mut preview, 10, 2);
    assert_eq!(text, ["   ▀▀▀▀", "   ▀▀▀▀"]);
    assert_eq!(buf[(3, 0)].fg, Color::Rgb(255, 0, 0));
    assert_eq!(buf[(3, 0)].bg, Color::Rgb(255, 0, 0));
    assert_eq!(buf[(5, 0)].fg, Color::Rgb(0, 255, 0));
    assert_eq!(buf[(3, 1)].fg, Color::Rgb(0, 0, 255));
    assert_eq!(buf[(6, 1)].bg, Color::Rgb(255, 255, 255));
    // A wide image in a tall area is centered vertically.
    let wide = png(&dir, "wide.png", 4, &[RED, RED, RED, RED]);
    let mut preview = loaded(&wide);
    let (_, text) = render(&mut preview, 4, 5);
    assert_eq!(text, ["", "", "▀▀▀▀", "", ""]);
}

#[test]
fn image_transparency_and_odd_heights() {
    let clear = [0, 0, 0, 0];
    let dir = TempDir::new("alpha");
    let path = png(&dir, "a.png", 2, &[clear, RED, GREEN, clear, BLUE, BLUE]);
    let mut preview = loaded(&path);
    let (buf, text) = render(&mut preview, 2, 2);
    assert_eq!(text, ["▄▀", "▀▀"]);
    assert_eq!(buf[(0, 0)].fg, Color::Rgb(0, 255, 0));
    assert_eq!(buf[(0, 0)].bg, Color::Reset);
    assert_eq!(buf[(0, 1)].bg, Color::Reset, "no pixel row below the last one");
}

#[test]
fn image_is_shown_as_an_image_where_the_terminal_can() {
    use crate::graphics::placeholder;
    let dir = TempDir::new("picture");
    let path = png(&dir, "tiny.png", 2, &[RED, GREEN, BLUE, WHITE]);
    let area = Rect::new(0, 0, 10, 4);
    // Images turned off: half blocks only.
    let mut plain = loaded(&path);
    assert!(!plain.show_image(area, (10, 4), &mut Buffer::empty(area)));
    let shared = std::sync::Arc::new(crate::graphics::Shared::new(true, 64 << 20));
    shared.set_cell(10, 20);
    let mut preview = Preview::open(&path);
    preview.set_graphics(shared.clone());
    preview.apply(super::Loaded::read(&path, true));
    assert!(preview.image().is_none(), "sent once shown");
    assert!(shared.used() > 0, "it counts in the images' memory");
    // 10×4 cells of 10×20 pixels: the square image fits in 80×80 pixels, 8 columns by 4 rows,
    // centred.
    let (mut buf, _) = render(&mut preview, 10, 4);
    let before = shared.version();
    assert!(preview.show_image(area, (10, 4), &mut buf));
    assert_ne!(shared.version(), before);
    let (kid, (generation, picture, placements)) = preview.image().unwrap();
    assert_eq!((picture.width, picture.height, placements), (2, 2, vec![(0, 8, 4)]));
    drop(picture);
    assert_eq!(buf[(1, 0)].symbol(), placeholder(kid, 0, 0));
    assert_eq!(buf[(8, 3)].symbol(), placeholder(kid, 3, 7));
    assert_eq!(buf[(1, 0)].fg, Color::Rgb((kid >> 16) as u8, (kid >> 8) as u8, kid as u8));
    assert_eq!((buf[(0, 0)].symbol(), buf[(9, 3)].symbol()), (" ", " "), "no half blocks left");
    // A smaller client shows the top left of the same placement: nothing changes.
    let small = Rect::new(0, 0, 5, 2);
    let mut cut = Buffer::empty(small);
    let before = shared.version();
    preview.show_image(small, (10, 4), &mut cut);
    assert_eq!(cut[(4, 1)].symbol(), placeholder(kid, 1, 3));
    assert_eq!((preview.image().unwrap().1.0, shared.version()), (generation, before));
    // A pane of another size is another placement.
    let half = Rect::new(0, 0, 10, 2);
    preview.show_image(half, (10, 2), &mut Buffer::empty(half));
    let (_, (later, _, placements)) = preview.image().unwrap();
    assert_eq!((later, placements), (generation + 1, vec![(0, 4, 2)]));
    // Closed: its memory is freed, and the terminals forget it.
    let before = shared.version();
    drop(preview);
    assert_ne!(shared.version(), before);
    assert_eq!(shared.used(), 0);
}

#[test]
fn an_image_beyond_the_images_memory_is_drawn_with_half_blocks() {
    let dir = TempDir::new("quota");
    let path = png(&dir, "tiny.png", 2, &[RED, GREEN, BLUE, WHITE]);
    // Less memory for images than any picture takes.
    let shared = std::sync::Arc::new(crate::graphics::Shared::new(true, 100));
    shared.set_cell(10, 20);
    let mut preview = Preview::open(&path);
    preview.set_graphics(shared.clone());
    preview.apply(super::Loaded::read(&path, true));
    assert_eq!(shared.used(), 0, "nothing beyond the quota");
    let (mut buf, text) = render(&mut preview, 10, 4);
    assert!(!preview.show_image(buf.area, (10, 4), &mut buf));
    assert!(text.iter().all(|row| row.contains('▀')), "{text:?}");
}

#[test]
fn image_sniffed_without_extension_and_decode_errors() {
    let dir = TempDir::new("sniff");
    let real = png(&dir, "real.png", 1, &[RED]);
    let disguised = dir.write("blob.dat", fs::read(real).unwrap());
    assert_eq!(loaded(&disguised).kind(), PreviewKind::Image);
    let broken = dir.write("broken.png", b"not an image at all");
    let mut preview = loaded(&broken);
    assert_eq!(preview.kind(), PreviewKind::Image);
    let (buf, text) = render(&mut preview, 40, 4);
    assert!(text.join(" ").contains("broken.png"), "{text:?}");
    assert_eq!(buf[(0, 0)].fg, Color::Red);
}

#[test]
fn directory_listing_order_and_styles() {
    let dir = TempDir::new("dir");
    fs::create_dir(dir.0.join("src")).unwrap();
    fs::create_dir(dir.0.join("Assets")).unwrap();
    fs::create_dir(dir.0.join(".git")).unwrap();
    dir.write("b.txt", vec![b'x'; 1536]);
    dir.write("a.txt", "hi");
    dir.write(".env", "");
    let mut preview = loaded(&dir.0);
    assert_eq!(preview.kind(), PreviewKind::Directory);
    let (buf, text) = render(&mut preview, 30, 8);
    let expected = [
        "           Assets/",
        "           src/",
        "      2 B  a.txt",
        "  1.5 KiB  b.txt",
        "           .git/",
        "      0 B  .env",
    ];
    assert_eq!(text[..6], expected);
    assert_eq!(buf[(11, 0)].fg, DIR_NAME.fg.unwrap());
    assert!(buf[(11, 4)].modifier.contains(Modifier::DIM), "hidden entries are dimmed");
    assert!(!buf[(11, 0)].modifier.contains(Modifier::DIM));
}

#[test]
fn human_sizes() {
    assert_eq!(human_size(0), "0 B");
    assert_eq!(human_size(1023), "1023 B");
    assert_eq!(human_size(1024), "1.0 KiB");
    assert_eq!(human_size(15 * 1024 * 1024), "15 MiB");
    assert_eq!(human_size(u64::MAX), "16 EiB");
}

#[test]
fn text_has_gutter_and_expanded_tabs() {
    let dir = TempDir::new("text");
    let path = dir.write("main.rs", "fn main() {\n\tprintln!(\"hi\");\r\n}\n");
    let mut preview = loaded(&path);
    assert_eq!(preview.kind(), PreviewKind::Text);
    let (buf, text) = render(&mut preview, 40, 5);
    assert_eq!(text[0], "  1 │ fn main() {");
    assert_eq!(text[1], "  2 │     println!(\"hi\");");
    assert_eq!(text[2], "  3 │ }");
    assert_eq!(text[3], "");
    assert_eq!(buf[(2, 0)].fg, gutter().fg.unwrap());
    assert_eq!(buf[(6, 0)].fg, Color::Reset);
    // No wrapping: long lines are cut at the edge.
    let (_, narrow) = render(&mut preview, 12, 3);
    assert_eq!(narrow[1], "  2 │     pr");
}

#[test]
fn text_shows_control_characters_and_empty_files() {
    let dir = TempDir::new("ctrl");
    let mut preview = loaded(&dir.write("c.txt", "a\x1bb"));
    let (_, text) = render(&mut preview, 20, 2);
    assert_eq!(text[0], "  1 │ a\u{241b}b");
    let mut empty = loaded(&dir.write("empty", ""));
    assert_eq!(empty.kind(), PreviewKind::Text);
    let (_, text) = render(&mut empty, 20, 2);
    assert_eq!(text[0], "(empty file)");
}

#[test]
fn binary_detection() {
    let dir = TempDir::new("binary");
    let mut nul = loaded(&dir.write("data", b"abc\0def"));
    assert_eq!(nul.kind(), PreviewKind::Binary);
    let (_, text) = render(&mut nul, 40, 2);
    assert_eq!(text[0], "binary file, 7 bytes");
    let latin1 = loaded(&dir.write("latin1.txt", b"caf\xe9"));
    assert_eq!(latin1.kind(), PreviewKind::Binary);
    let big = loaded(&dir.write("big", vec![0u8; 2048]));
    assert_eq!(big.visible_text(), "binary file, 2048 bytes (2.0 KiB)");
}

#[test]
fn scrolling_is_clamped() {
    let dir = TempDir::new("scroll");
    let body: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    let mut preview = loaded(&dir.write("long.txt", body));
    let (_, text) = render(&mut preview, 30, 10);
    assert_eq!(text[0], "  1 │ line 1");
    assert!(text[9].ends_with(" 0%"), "{:?}", text[9]);
    preview.scroll(-5);
    assert_eq!(preview.visible_text().lines().next(), Some("  1 │ line 1"));
    preview.scroll(1000);
    assert_eq!(preview.visible_text().lines().next(), Some(" 91 │ line 91"));
    preview.top();
    preview.page_down();
    assert_eq!(preview.visible_text().lines().next(), Some(" 11 │ line 11"));
    preview.page_up();
    preview.page_up();
    assert_eq!(preview.visible_text().lines().next(), Some("  1 │ line 1"));
    preview.bottom();
    let (buf, text) = render(&mut preview, 30, 10);
    assert_eq!(text[0], " 91 │ line 91");
    assert_eq!(text[9], "100 │ line 100           100%");
    assert_eq!(buf[(29, 9)].fg, indicator().fg.unwrap());
    // Growing the area clamps the offset again and hides the indicator.
    let (_, text) = render(&mut preview, 30, 200);
    assert_eq!(text[0], "  1 │ line 1");
    assert!(!text.iter().any(|l| l.contains('%')));
}

#[test]
fn scrolling_before_render_and_on_images() {
    let dir = TempDir::new("prerender");
    let body: String = (1..=50).map(|i| format!("{i}\n")).collect();
    let mut preview = loaded(&dir.write("n.txt", body));
    preview.bottom();
    preview.scroll(3);
    let (_, text) = render(&mut preview, 20, 10);
    assert_eq!(text[0], " 41 │ 41");
    let mut image = loaded(&png(&dir, "i.png", 1, &[RED]));
    image.scroll(10);
    image.bottom();
    image.page_down();
    let (_, text) = render(&mut image, 4, 2);
    assert_eq!(text, ["▀▀▀▀", "▀▀▀▀"]);
}

#[test]
fn zero_sized_and_offscreen_areas_do_not_panic() {
    let dir = TempDir::new("zero");
    let paths = [
        dir.write("a.md", "# x\n\n| a |\n|---|\n| b |\n"),
        dir.write("b.txt", "text"),
        dir.write("c", b"\0\0"),
        png(&dir, "d.png", 1, &[RED]),
        dir.0.clone(),
        dir.0.join("missing"),
    ];
    let mut buf = Buffer::empty(Rect::new(0, 0, 5, 5));
    for path in &paths {
        let mut preview = loaded(path);
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 0, 5),
            Rect::new(0, 0, 5, 0),
            Rect::new(1, 1, 1, 1),
            Rect::new(4, 4, 10, 10),
            Rect::new(50, 50, 10, 10),
        ] {
            preview.render(area, &mut buf);
            preview.scroll(-3);
            preview.page_down();
            preview.bottom();
            let _ = preview.visible_text();
        }
    }
}

#[test]
fn render_stays_inside_area() {
    let dir = TempDir::new("inside");
    let long: String = (0..50).map(|i| format!("a very long line number {i} 漢字漢字\n")).collect();
    let paths = [
        dir.write("a.md", format!("# Title\n\n{long}\n\n```\n{long}```\n")),
        dir.write("b.txt", &long),
        png(&dir, "c.png", 2, &[RED, GREEN, BLUE, WHITE]),
        dir.0.clone(),
    ];
    let outer = Rect::new(0, 0, 30, 12);
    let area = Rect::new(5, 3, 17, 6);
    for path in &paths {
        let mut buf = Buffer::filled(outer, ratatui::buffer::Cell::new("X"));
        let mut preview = loaded(path);
        preview.bottom();
        preview.render(area, &mut buf);
        for y in outer.top()..outer.bottom() {
            for x in outer.left()..outer.right() {
                if !area.contains((x, y).into()) {
                    assert_eq!(buf[(x, y)].symbol(), "X", "{path:?} wrote at {x},{y}");
                }
            }
        }
    }
}

#[test]
fn unreadable_path_renders_red_error() {
    let mut preview = loaded(Path::new("/definitely/not/here.txt"));
    assert_eq!(preview.kind(), PreviewKind::Text);
    assert_eq!(preview.title(), "here.txt");
    let (buf, text) = render(&mut preview, 30, 5);
    assert!(text[0].starts_with("cannot read"), "{text:?}");
    assert!(text.iter().all(|l| l.width() <= 30));
    assert_eq!(buf[(0, 0)].fg, Color::Red);
    assert!(!preview.modified_since_load());
}

/// Wait for the preview's watcher to see a change, then read the file as the server does.
fn read_when_changed(preview: &mut Preview) {
    let started = std::time::Instant::now();
    while !preview.modified_since_load() {
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "the change was not seen");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    preview.apply(super::Loaded::read(preview.path(), false));
}

#[test]
fn reload_follows_file_changes() {
    let dir = TempDir::new("reload");
    let path = dir.write("notes.txt", "first\n");
    // Opening reads nothing on this thread: the server reads the file elsewhere.
    let mut preview = Preview::open(&path);
    assert!(preview.modified_since_load());
    assert_eq!(preview.visible_text(), "loading…");
    preview.apply(super::Loaded::read(&path, false));
    assert!(!preview.modified_since_load());
    fs::write(&path, "second version\n").unwrap();
    read_when_changed(&mut preview);
    assert!(!preview.modified_since_load());
    assert_eq!(preview.visible_text(), "  1 │ second version");
    // `r` asks for a read, done off the server's thread too.
    preview.reload();
    assert!(preview.modified_since_load());
    fs::remove_file(&path).unwrap();
    read_when_changed(&mut preview);
    assert!(preview.visible_text().starts_with("cannot read"));
}

#[test]
fn a_change_of_the_file_is_told() {
    let dir = TempDir::new("told");
    let path = dir.write("notes.txt", "first\n");
    let preview = Preview::open(&path);
    let (tx, rx) = crossbeam_channel::unbounded();
    preview.on_change(move || {
        let _ = tx.send(());
    });
    let mut preview = preview;
    preview.apply(super::Loaded::read(&path, false));
    // Nothing changed: nothing is told.
    assert!(rx.recv_timeout(std::time::Duration::from_millis(1500)).is_err());
    fs::write(&path, "second version\n").unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the change was told");
    assert!(preview.modified_since_load());
}

#[test]
fn big_markdown_says_it_is_cut() {
    let dir = TempDir::new("bigmd");
    let mut body = String::from("# Title\n\n");
    while body.len() as u64 <= super::MAX_TEXT_BYTES {
        body.push_str("a line of text that repeats\n\n");
    }
    let path = dir.write("big.md", body.as_str());
    let mut preview = loaded(&path);
    preview.bottom();
    let (_, text) = render(&mut preview, 60, 4);
    assert!(text.iter().any(|l| l.contains("truncated, file is")), "{text:?}");
}

#[test]
fn titles() {
    assert_eq!(loaded(Path::new("/tmp/x/README.md")).title(), "README.md");
    assert_eq!(loaded(Path::new("/")).title(), "/");
    let dir = TempDir::new("title");
    let name = dir.0.file_name().unwrap().to_string_lossy().into_owned();
    fs::create_dir(dir.0.join("sub")).unwrap();
    assert_eq!(loaded(&dir.0.join("sub/..")).title(), name);
}

#[test]
fn wide_tables_shrink_in_linear_time() {
    use super::markdown::fit_columns;
    let mut widths = vec![10, 3, 40, 7];
    fit_columns(&mut widths, 30);
    assert_eq!(widths.iter().sum::<usize>(), 30);
    assert_eq!(widths, [10, 3, 10, 7], "the widest give way first");
    let mut tight = vec![10, 10];
    fit_columns(&mut tight, 2);
    assert_eq!(tight, [super::MIN_COLUMN_WIDTH; 2], "never below the minimum");
    // A thousand columns of thousands of characters: immediate, not billions of steps.
    let mut many = vec![8_000; 1_000];
    let started = std::time::Instant::now();
    fit_columns(&mut many, 5_000);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(many.iter().all(|w| *w == 5), "{:?}", &many[..3]);
    let mut fits = vec![5, 6];
    fit_columns(&mut fits, 80);
    assert_eq!(fits, [5, 6]);
}

#[test]
fn titles_come_from_the_path_alone() {
    // A directory named through `..` gets its name without asking the file system, which may hang.
    assert_eq!(Preview::open(Path::new("/srv/missing-mount/x/..")).title(), "missing-mount");
    assert_eq!(Preview::open(Path::new("/srv/notes.md")).title(), "notes.md");
    assert_eq!(Preview::open(Path::new("/")).title(), "/");
}

#[test]
fn a_preview_callback_returns_with_both_notification_queues_full() {
    let preview = Preview::of_text("test", "unchanged");
    let (tx, rx) = crate::server::event_channel(1);
    assert!(tx.wake());
    assert!(tx.wake());
    let sender = tx.clone();
    preview.on_change(move || {
        let _ = sender.wake();
    });
    let call = || {
        let watch = preview.watch.clone();
        std::thread::spawn(move || {
            let callback = watch.changed.lock().unwrap();
            callback.as_ref().unwrap()();
        })
        .join()
        .unwrap();
    };
    call();
    assert!(rx.reconcile(false).wake);
    // The callback also returns during teardown, still holding its callback mutex.
    drop(rx);
    call();
}
