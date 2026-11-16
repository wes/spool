//! Turns message bodies into a flat list of styled [`Block`]s.
//!
//! Spool does not embed a browser. A message is laid out with the same text
//! system as the rest of the app, which is what keeps opening a message
//! instant and makes mail look like it belongs in the window instead of like a
//! web page bolted into it. That means reducing HTML mail to structure —
//! headings, paragraphs, quotes, lists, links, emphasis — plus the part of the
//! sender's presentation that carries meaning: the green on a `RESOLVED`
//! badge, the red on an error count, the monospace on a service name.
//!
//! Those colours are kept **raw** here. Whether one is legible depends on the
//! theme it lands in, which this crate knows nothing about; the reader adapts
//! them. What is dropped is the presentation that carries no meaning — page
//! backgrounds, fixed widths, fonts nobody has.
//!
//! The parser is deliberately tolerant: email HTML is frequently malformed,
//! and the only unacceptable outcome is failing to show the message.

/// A colour exactly as the sender wrote it.
///
/// Raw on purpose: `#5c554c` is a legible muted grey on the near-black the
/// sender designed for and invisible on a white one. Deciding that is the
/// reader's job, which has a theme to compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// 0 is fully transparent, 255 fully opaque. Mail draws badge fills as a
    /// colour at 15% over its own background, so the alpha carries as much of
    /// the intent as the hue.
    pub a: u8,
}

/// Character-level styling within a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpanStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    /// The sender's text colour, if they set one.
    pub color: Option<Rgba>,
    /// The fill behind the text — a badge. Only ever taken from an inline
    /// element: a background on a `<div>` or a `<td>` is a card behind a whole
    /// region, not a highlight behind words.
    pub background: Option<Rgba>,
    /// The sender asked for a monospace family. Worth keeping because mail
    /// uses it to mean something — an identifier, a code, a log line.
    pub mono: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: SpanStyle,
    /// Target of the enclosing `<a href>`, if any.
    pub link: Option<String>,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: SpanStyle::default(), link: None }
    }
}

/// Where an image's bytes come from.
///
/// Kept as a description rather than the bytes themselves: this crate does no
/// I/O, and the difference between the three matters to the reader. A `Cid`
/// arrived with the message and can be shown immediately; a `Remote` costs a
/// request to the sender's server, which is how tracking pixels work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// `cid:...`, referring to a part of this same message.
    Cid(String),
    /// An absolute `http(s)` URL.
    Remote(String),
    /// A `data:` URL. Still encoded — decoding is the caller's job, so that
    /// this crate keeps no base64 of its own.
    Data { mime: String, base64: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub source: ImageSource,
    /// The `alt` text, shown while the image is unloaded or unavailable.
    pub alt: String,
    /// Sizes the sender declared, used to reserve space before the bytes are
    /// decoded. Absent means "whatever the image turns out to be".
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// One table cell, flattened to spans.
pub type Cell = Vec<Span>;

/// A table that carries data rather than layout — see [`is_data_table`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// Empty when the table had no header row.
    pub headers: Vec<Cell>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    /// The widest row, which is how many columns the UI has to draw.
    pub fn columns(&self) -> usize {
        self.rows
            .iter()
            .map(Vec::len)
            .chain(std::iter::once(self.headers.len()))
            .max()
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Span>),
    Heading { level: u8, spans: Vec<Span> },
    /// `depth` is the nesting level of `>` or `<blockquote>`; the UI draws one
    /// gutter bar per level.
    Quote { depth: u8, spans: Vec<Span> },
    ListItem { depth: u8, ordered: bool, index: usize, spans: Vec<Span> },
    Code(String),
    Image(Image),
    Table(Table),
    Rule,
}

impl Block {
    /// The block's own spans. A table's are in its cells; see [`Self::text`].
    pub fn spans(&self) -> &[Span] {
        match self {
            Block::Paragraph(s)
            | Block::Heading { spans: s, .. }
            | Block::Quote { spans: s, .. }
            | Block::ListItem { spans: s, .. } => s,
            Block::Code(_) | Block::Rule | Block::Image(_) | Block::Table(_) => &[],
        }
    }

    /// All the readable text in the block, cells included.
    ///
    /// Used for previews and search, where a receipt's table is exactly the
    /// part worth showing.
    pub fn text(&self) -> String {
        match self {
            Block::Code(code) => code.clone(),
            Block::Image(image) => image.alt.clone(),
            Block::Table(table) => {
                let cell_text = |cell: &Cell| {
                    cell.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("")
                };
                let row_text = |row: &Vec<Cell>| {
                    row.iter().map(cell_text).collect::<Vec<_>>().join(" ")
                };
                std::iter::once(&table.headers)
                    .filter(|h| !h.is_empty())
                    .map(row_text)
                    .chain(table.rows.iter().map(row_text))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            Block::Rule => String::new(),
            other => other.spans().iter().map(|s| s.text.as_str()).collect(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            Block::Rule => false,
            // An image is never empty: it is the content.
            Block::Image(_) => false,
            Block::Code(text) => text.trim().is_empty(),
            Block::Table(table) => table.rows.is_empty() && table.headers.is_empty(),
            other => other.spans().iter().all(|s| is_blank(&s.text)),
        }
    }
}

/// Which of a message's two bodies to lay out.
///
/// Multipart/alternative mail carries both. The HTML part is what the sender
/// actually composed — the plain part is a lossy transcription of it, often an
/// automated one with the links torn out and the tables flattened — so it is
/// the default. The plain part stays reachable for the messages whose HTML is
/// a mess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    Rich,
    Plain,
}

impl Source {
    pub fn other(self) -> Self {
        match self {
            Source::Rich => Source::Plain,
            Source::Plain => Source::Rich,
        }
    }
}

/// Whether a message has both bodies, and so whether offering a choice between
/// them means anything.
pub fn has_both(plain: &str, html: &str) -> bool {
    !plain.trim().is_empty() && is_document(html)
}

/// Whether an HTML part is a document, as opposed to a message that happens to
/// have been put in an envelope.
///
/// Mail routinely carries an HTML part that is an empty shell, one line inside
/// `<html><body>`, or the plain text with no tags in it at all. None of those
/// is a document: there is no layout in them to preserve, and treating them as
/// one puts a rendered page in front of a message that never had one — a white
/// slab in a dark window, showing what the plain-text body would have shown in
/// the reader's own colours.
///
/// So the test is not "are there bytes" but "is there any element that does
/// something". The skeleton a mail library wraps around a line of text does
/// not count.
pub fn is_document(html: &str) -> bool {
    /// Containers and line breaks: they hold things, they do not do anything
    /// to them. A message built only from these has no formatting to lose.
    ///
    /// Deliberately not the inline marks — a `<b>`, an `<a>` or a `<font>`
    /// changes how a word reads, and that is worth a rendered page. The line
    /// is "does any element affect the text", not "is there any tag".
    const SKELETON: &[&str] =
        &["html", "head", "body", "meta", "title", "br", "div", "span", "p", "o:p"];

    let bytes = html.as_bytes();
    let mut pos = 0;

    while let Some(next) = bytes[pos..].iter().position(|&b| b == b'<') {
        pos += next + 1;
        let name: String = html[pos..]
            .chars()
            .skip_while(|c| *c == '/')
            .take_while(|c| c.is_ascii_alphanumeric() || *c == ':')
            .flat_map(char::to_lowercase)
            .collect();

        if !name.is_empty() && !SKELETON.contains(&name.as_str()) {
            return true;
        }
    }

    false
}

/// Which body a request actually resolves to.
///
/// Separate from [`body_to_blocks`] so a caller can label what it is showing.
/// Asking for one a message does not have falls back rather than failing — an
/// empty reader is never the right answer to "show me this message" — and the
/// label has to say what was really used, not what was asked for.
pub fn effective_source(plain: &str, html: &str, requested: Source) -> Source {
    // A wrapper around a line of text is not an HTML body worth preferring
    // over the plain one; see [`is_document`].
    let (has_plain, has_html) = (!plain.trim().is_empty(), is_document(html));

    match requested {
        Source::Rich if has_html => Source::Rich,
        Source::Plain if has_plain => Source::Plain,
        // Neither preference can be honoured; take whatever exists.
        _ if has_plain => Source::Plain,
        _ => Source::Rich,
    }
}

/// Renders a body to blocks.
pub fn body_to_blocks(plain: &str, html: &str, source: Source) -> Vec<Block> {
    match effective_source(plain, html, source) {
        // Still `html.trim()` rather than `is_document`: a message whose only
        // body is a bare line in an HTML part has to come from somewhere.
        Source::Rich if !html.trim().is_empty() => html_to_blocks(html),
        Source::Plain if !plain.trim().is_empty() => plain_to_blocks(plain),
        // Nothing to show at all.
        _ => Vec::new(),
    }
}

/// Parses plain-text mail, honouring `>` quoting and linkifying bare URLs.
pub fn plain_to_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut para_depth = 0u8;

    let flush = |blocks: &mut Vec<Block>, para: &mut Vec<String>, depth: u8| {
        if para.is_empty() {
            return;
        }
        let joined = para.join(" ");
        para.clear();
        let spans = linkify(&joined, SpanStyle::default());
        blocks.push(if depth > 0 {
            Block::Quote { depth, spans }
        } else {
            Block::Paragraph(spans)
        });
    };

    for raw_line in text.lines() {
        let (depth, line) = strip_quote_markers(raw_line);
        let trimmed = line.trim_end();

        if trimmed.trim().is_empty() {
            flush(&mut blocks, &mut para, para_depth);
            continue;
        }

        // A signature delimiter, and the `-- ` before it, are structure worth
        // keeping visible.
        if trimmed.trim() == "--" || trimmed.trim() == "-- " {
            flush(&mut blocks, &mut para, para_depth);
            blocks.push(Block::Rule);
            continue;
        }

        if depth != para_depth {
            flush(&mut blocks, &mut para, para_depth);
            para_depth = depth;
        }

        if let Some((ordered, index, rest)) = list_marker(trimmed) {
            flush(&mut blocks, &mut para, para_depth);
            blocks.push(Block::ListItem {
                depth: 0,
                ordered,
                index,
                spans: linkify(rest, SpanStyle::default()),
            });
            continue;
        }

        para.push(trimmed.trim().to_string());
    }
    flush(&mut blocks, &mut para, para_depth);

    blocks
}

/// Counts and removes leading `>` quote markers.
fn strip_quote_markers(line: &str) -> (u8, &str) {
    let mut depth = 0u8;
    let mut rest = line;
    loop {
        let candidate = rest.trim_start();
        match candidate.strip_prefix('>') {
            Some(after) if depth < 8 => {
                depth += 1;
                rest = after;
            }
            _ => return (depth, rest),
        }
    }
}

/// Recognizes `- item`, `* item` and `1. item`.
fn list_marker(line: &str) -> Option<(bool, usize, &str)> {
    let t = line.trim_start();
    for bullet in ["- ", "* ", "\u{2022} "] {
        if let Some(rest) = t.strip_prefix(bullet) {
            return Some((false, 0, rest));
        }
    }
    let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        let rest = &t[digits.len()..];
        for sep in [". ", ") "] {
            if let Some(rest) = rest.strip_prefix(sep) {
                return Some((true, digits.parse().unwrap_or(0), rest));
            }
        }
    }
    None
}

/// Splits text into spans, promoting anything that looks like a URL to a link.
fn linkify(text: &str, style: SpanStyle) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut rest = text;

    while let Some(start) = find_url(rest) {
        if start > 0 {
            spans.push(Span { text: rest[..start].to_string(), style, link: None });
        }
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '<' || c == '>' || c == '"')
            .unwrap_or(tail.len());
        // Trailing punctuation belongs to the sentence, not the URL.
        let url = tail[..end].trim_end_matches(['.', ',', ')', ';', ':', '!', '?', ']']);
        let url_len = url.len();

        spans.push(Span {
            text: url.to_string(),
            style,
            link: Some(url.to_string()),
        });
        rest = &tail[url_len..];
    }

    if !rest.is_empty() {
        spans.push(Span { text: rest.to_string(), style, link: None });
    }
    if spans.is_empty() {
        spans.push(Span { text: String::new(), style, link: None });
    }
    spans
}

fn find_url(text: &str) -> Option<usize> {
    ["https://", "http://", "mailto:"]
        .iter()
        .filter_map(|scheme| text.find(scheme))
        .min()
}

/// Reduces HTML mail to blocks.
pub fn html_to_blocks(html: &str) -> Vec<Block> {
    let mut p = HtmlParser {
        input: html.as_bytes(),
        pos: 0,
        blocks: Vec::new(),
        pending: Vec::new(),
        style: SpanStyle::default(),
        style_stack: Vec::new(),
        link: None,
        quote_depth: 0,
        list_stack: Vec::new(),
        heading: None,
        in_pre: false,
        depth: 0,
    };
    p.run();
    p.finish()
}

/// How deep nested tables may go before their contents are dropped.
///
/// Layout tables nest three or four deep routinely and pathologically deep
/// occasionally; this bounds the recursion without truncating real mail.
const MAX_TABLE_DEPTH: u8 = 12;

fn html_to_blocks_at(html: &str, depth: u8) -> Vec<Block> {
    if depth > MAX_TABLE_DEPTH {
        return Vec::new();
    }
    let mut p = HtmlParser {
        input: html.as_bytes(),
        pos: 0,
        blocks: Vec::new(),
        pending: Vec::new(),
        style: SpanStyle::default(),
        style_stack: Vec::new(),
        link: None,
        quote_depth: 0,
        list_stack: Vec::new(),
        heading: None,
        in_pre: false,
        depth,
    };
    p.run();
    p.finish()
}

struct ListLevel {
    ordered: bool,
    next_index: usize,
}

struct HtmlParser<'a> {
    input: &'a [u8],
    pos: usize,
    blocks: Vec<Block>,
    pending: Vec<Span>,
    style: SpanStyle,
    style_stack: Vec<SpanStyle>,
    link: Option<String>,
    quote_depth: u8,
    list_stack: Vec<ListLevel>,
    heading: Option<u8>,
    in_pre: bool,
    /// Nesting level, incremented for each table re-entered.
    depth: u8,
}

impl<'a> HtmlParser<'a> {
    fn run(&mut self) {
        while self.pos < self.input.len() {
            if self.input[self.pos] == b'<' {
                if !self.try_tag() {
                    // A stray `<` that is not a tag. Treat it as text.
                    self.push_text("<");
                    self.pos += 1;
                }
            } else {
                let start = self.pos;
                while self.pos < self.input.len() && self.input[self.pos] != b'<' {
                    self.pos += 1;
                }
                let text = String::from_utf8_lossy(&self.input[start..self.pos]).into_owned();
                self.push_text(&decode_entities(&text));
            }
        }
    }

    /// Parses one tag at `self.pos`. Returns false if it does not look like a
    /// tag after all, leaving `self.pos` untouched.
    fn try_tag(&mut self) -> bool {
        let rest = &self.input[self.pos..];

        if rest.starts_with(b"<!--") {
            self.pos += find_sub(rest, b"-->").map_or(rest.len(), |ix| ix + 3);
            return true;
        }
        if rest.starts_with(b"<!") || rest.starts_with(b"<?") {
            self.pos += rest.iter().position(|&b| b == b'>').map_or(rest.len(), |ix| ix + 1);
            return true;
        }

        let Some(close) = rest.iter().position(|&b| b == b'>') else {
            return false;
        };
        let inner = String::from_utf8_lossy(&rest[1..close]).into_owned();
        let closing = inner.starts_with('/');
        let inner_trimmed = inner.trim_start_matches('/');

        let name: String = inner_trimmed
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if name.is_empty() {
            return false;
        }
        self.pos += close + 1;

        // An element the sender hid is not content. Marketing mail always
        // opens with a hidden preheader — the teaser the inbox list shows —
        // and rendering it means every newsletter starts by saying the same
        // sentence twice.
        if !closing && is_hidden(inner_trimmed) {
            // A void element has no subtree; skipping to a closing tag it
            // never has would swallow the rest of the message.
            if !is_void(&name) && !inner_trimmed.trim_end().ends_with('/') {
                self.pos = match_end(self.input, self.pos, name.as_bytes()).1;
            }
            return true;
        }

        // `<script>`/`<style>` content is markup, not prose — skip the body wholesale.
        if !closing && matches!(name.as_str(), "script" | "style" | "head" | "title") {
            let end_tag = format!("</{name}");
            let remaining = &self.input[self.pos..];
            self.pos += match find_sub_ci(remaining, end_tag.as_bytes()) {
                Some(ix) => {
                    let after = &remaining[ix..];
                    ix + after.iter().position(|&b| b == b'>').map_or(after.len(), |i| i + 1)
                }
                None => remaining.len(),
            };
            return true;
        }

        if closing {
            self.close(&name);
        } else {
            let self_closing = inner_trimmed.trim_end().ends_with('/');
            self.open(&name, inner_trimmed, self_closing);
        }
        true
    }

    fn open(&mut self, name: &str, raw: &str, self_closing: bool) {
        // Every element that has a subtree owns exactly one style frame, so a
        // colour set on a `<span>`, a `<td>` or a `<font>` lasts precisely as
        // long as the element does. Void and self-closing elements have no
        // subtree and take none.
        let subtree = !is_void(name) && !self_closing;
        if subtree {
            self.style_stack.push(self.style);
        }

        match name {
            "br" => self.flush_block(),
            "hr" => {
                self.flush_block();
                self.blocks.push(Block::Rule);
            }
            // Tables are handled whole rather than as a stream of open and
            // close tags: whether one is layout or data is a property of the
            // finished table, not of anything visible at `<table>`.
            "table" => self.take_table(),
            "p" | "div" | "tr" | "section" | "article" | "header" | "footer" => {
                self.flush_block()
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_block();
                self.heading = name[1..].parse().ok();
            }
            "blockquote" => {
                self.flush_block();
                self.quote_depth = self.quote_depth.saturating_add(1).min(8);
            }
            "ul" | "ol" => {
                self.flush_block();
                self.list_stack.push(ListLevel { ordered: name == "ol", next_index: 1 });
            }
            "li" => self.flush_block(),
            "pre" => {
                self.flush_block();
                self.in_pre = true;
            }
            "b" | "strong" => self.style.bold = true,
            "i" | "em" => self.style.italic = true,
            "u" | "ins" => self.style.underline = true,
            "s" | "del" | "strike" => self.style.strike = true,
            "code" | "tt" | "kbd" | "samp" => self.style.code = true,
            "a" => self.link = attr(raw, "href").filter(|h| !h.starts_with("javascript:")),
            "img" => self.push_image(raw),
            "td" | "th" => self.push_text(" "),
            _ => {}
        }

        // The sender's own styling, applied on top of what the tag means.
        if subtree {
            self.apply_style(name, raw);
        }
    }

    /// Reads the presentation the sender asked for off one tag.
    ///
    /// Only the declarations that carry meaning. A page background, a fixed
    /// width or a font stack this machine does not have says nothing about the
    /// message, and honouring it would make mail look like a web page that had
    /// been dropped into the window.
    fn apply_style(&mut self, name: &str, raw: &str) {
        // `<font color=…>` predates CSS and is still in mail from systems that
        // have not been touched since.
        if name == "font"
            && let Some(color) = attr(raw, "color").as_deref().and_then(parse_color)
        {
            self.style.color = Some(color);
        }

        let Some(style) = attr(raw, "style") else { return };
        for (property, value) in declarations(&style) {
            match property.as_str() {
                "color" => {
                    if let Some(color) = parse_color(&value) {
                        self.style.color = Some(color);
                    }
                }
                // A fill behind words is a badge; a fill behind a region is a
                // card, and a card drawn as a highlight behind every line in
                // it is worse than no card at all.
                "background" | "background-color" if is_inline(name) => {
                    if let Some(color) = parse_color(&value) {
                        self.style.background = Some(color);
                    }
                }
                "font-weight" => self.style.bold = is_bold(&value),
                "font-style" => self.style.italic = value.starts_with("italic"),
                "font-family" => self.style.mono = is_monospace(&value),
                "text-decoration" | "text-decoration-line" => {
                    self.style.underline = value.contains("underline");
                    self.style.strike = value.contains("line-through");
                }
                _ => {}
            }
        }
    }

    fn close(&mut self, name: &str) {
        match name {
            "p" | "div" | "tr" | "section" | "article" | "header" | "footer" | "li" => {
                if name == "li" {
                    self.flush_list_item();
                } else {
                    self.flush_block();
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_block();
                self.heading = None;
            }
            "blockquote" => {
                self.flush_block();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            "ul" | "ol" => {
                self.flush_block();
                self.list_stack.pop();
            }
            "pre" => {
                self.flush_block();
                self.in_pre = false;
            }
            "a" => self.link = None,
            _ => {}
        }

        // The frame this element opened. A stray closing tag pops an outer
        // frame instead, which is the same tolerance the rest of this parser
        // shows malformed mail: styling ends early rather than the message
        // failing to render.
        if !is_void(name) {
            self.pop_style();
        }
    }

    /// Emits an `<img>` as its own block, or drops it.
    ///
    /// Images in mail are block-level in practice — banners, signatures,
    /// product shots — so they get their own block rather than trying to flow
    /// inside a paragraph, which the text system cannot do anyway.
    fn push_image(&mut self, raw: &str) {
        let alt = attr(raw, "alt").unwrap_or_default().trim().to_string();
        let width = attr(raw, "width").as_deref().and_then(parse_dimension);
        let height = attr(raw, "height").as_deref().and_then(parse_dimension);

        // A 1x1 image is a tracking pixel, and a few pixels of one dimension
        // is a spacer holding a table cell open. Neither is content, and both
        // would otherwise become a visible box in the reader.
        if is_spacer(width, height) {
            return;
        }

        let source = attr(raw, "src").as_deref().and_then(parse_image_source);
        match source {
            Some(source) => {
                self.flush_block();
                self.blocks.push(Block::Image(Image { source, alt, width, height }));
            }
            // No usable source: the alt text is all that is left of it.
            None if !alt.is_empty() => self.push_text(&format!("[{alt}]")),
            None => {}
        }
    }

    /// Consumes a `<table>` through its matching `</table>` and emits either a
    /// grid or the cells' contents inline.
    fn take_table(&mut self) {
        self.flush_block();

        let (inner, after) = match_end(self.input, self.pos, b"table");
        let html = String::from_utf8_lossy(&self.input[self.pos..inner]).into_owned();
        self.pos = after;

        if self.depth >= MAX_TABLE_DEPTH {
            return;
        }

        let rows = split_rows(html.as_bytes());
        let mut parsed: Vec<Row> = rows
            .into_iter()
            .map(|row| Row {
                header: row.header,
                cells: row
                    .cells
                    .iter()
                    .map(|cell| html_to_blocks_at(cell, self.depth + 1))
                    .collect(),
            })
            .collect();

        // A row whose cells are all empty is not a row of the message. Mail
        // draws bar charts as table cells with a background colour and a
        // height and no content, and spacers the same way; rendering those as
        // blank grid lines claims the sender sent an empty row.
        parsed.retain(|row| row.cells.iter().any(|cell| !cell.is_empty()));
        if parsed.is_empty() {
            return;
        }

        if is_data_table(&parsed) {
            self.blocks.push(Block::Table(to_table(&parsed)));
        } else {
            // Layout: the table was scaffolding, so keep the contents and drop
            // the scaffolding. This is what turns a newsletter from a column
            // of one-cell boxes into a page.
            for row in parsed {
                match join_band(&row) {
                    Some(spans) => self.blocks.push(Block::Paragraph(spans)),
                    None => {
                        for cell in row.cells {
                            self.blocks.extend(cell);
                        }
                    }
                }
            }
        }
    }

    fn pop_style(&mut self) {
        if let Some(prev) = self.style_stack.pop() {
            self.style = prev;
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

        let normalized = if self.in_pre { text.to_string() } else { collapse_ws(text) };
        if normalized.is_empty() {
            return;
        }
        // Don't open a block with a space left over from collapsed markup.
        if self.pending.is_empty() && is_blank(&normalized) {
            return;
        }

        for span in linkify(&normalized, self.style) {
            if span.text.is_empty() {
                continue;
            }
            let link = self.link.clone().or(span.link);
            // Merge into the previous span when nothing changed — fewer spans
            // means fewer text runs to shape at paint time.
            match self.pending.last_mut() {
                Some(last) if last.style == span.style && last.link == link => {
                    last.text.push_str(&span.text)
                }
                _ => self.pending.push(Span { text: span.text, style: span.style, link }),
            }
        }
    }

    fn take_pending(&mut self) -> Option<Vec<Span>> {
        while self.pending.last().is_some_and(|s| is_blank(&s.text)) {
            self.pending.pop();
        }
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }

    fn flush_block(&mut self) {
        let Some(spans) = self.take_pending() else { return };

        let block = if self.in_pre {
            Block::Code(spans.iter().map(|s| s.text.as_str()).collect())
        } else if let Some(level) = self.heading {
            Block::Heading { level, spans }
        } else if self.quote_depth > 0 {
            Block::Quote { depth: self.quote_depth, spans }
        } else {
            Block::Paragraph(spans)
        };

        if !block.is_empty() {
            self.blocks.push(block);
        }
    }

    fn flush_list_item(&mut self) {
        let Some(spans) = self.take_pending() else { return };

        let depth = self.list_stack.len().saturating_sub(1) as u8;
        let (ordered, index) = match self.list_stack.last_mut() {
            Some(level) => {
                let ix = level.next_index;
                level.next_index += 1;
                (level.ordered, ix)
            }
            None => (false, 0),
        };
        self.blocks.push(Block::ListItem { depth, ordered, index, spans });
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_block();
        self.blocks.retain(|b| !b.is_empty());
        self.blocks
    }
}

/// Collapses runs of whitespace, as HTML layout would.
/// Collapses runs of whitespace, and drops the invisible padding along with
/// them.
///
/// A joiner is kept only where it does work: between two visible characters.
/// That keeps `👨\u{200d}👩\u{200d}👧` one emoji and keeps Persian and Indic
/// words intact, while a run of them next to spaces — which is what preheader
/// padding is — collapses to nothing.
fn collapse_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_was_space = false;
    let mut previous_visible = false;

    let chars: Vec<char> = text.chars().collect();
    for (ix, ch) in chars.iter().copied().enumerate() {
        if is_invisible(ch) {
            continue;
        }

        if is_joiner(ch) {
            let next_visible = chars[ix + 1..]
                .iter()
                .copied()
                .find(|c| !is_invisible(*c))
                .is_some_and(|c| !c.is_whitespace() && !is_joiner(c));
            if previous_visible && next_visible {
                out.push(ch);
            }
            continue;
        }

        if ch.is_whitespace() || ch == '\u{a0}' {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
            previous_visible = false;
        } else {
            out.push(ch);
            last_was_space = false;
            previous_visible = true;
        }
    }

    out
}

/// Walks a tag's attributes as `(name, value)` pairs.
///
/// Hand-rolled rather than searching the raw tag for a name, because an
/// attribute value contains everything a tag does: quotes, colons,
/// semicolons, even angle brackets. Searching finds the name *inside* those
/// values — which is how `style="overflow: hidden"` came to be read as the
/// `hidden` attribute, and `data-src` as `src`.
///
/// Names come back lowercased; a valueless attribute comes back as `None`.
fn attributes(raw: &str) -> Vec<(String, Option<String>)> {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;

    // Step over the element name, which is not an attribute.
    while i < chars.len() && !chars[i].is_whitespace() {
        i += 1;
    }

    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == '/') {
            i += 1;
        }
        let start = i;
        while i < chars.len()
            && !chars[i].is_whitespace()
            && chars[i] != '='
            && chars[i] != '/'
        {
            i += 1;
        }
        if i == start {
            // Nothing consumed — a stray character. Step past it rather than
            // spinning on it.
            i += 1;
            continue;
        }
        let name: String = chars[start..i].iter().flat_map(|c| c.to_lowercase()).collect();

        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if chars.get(j) != Some(&'=') {
            out.push((name, None));
            i = j;
            continue;
        }

        j += 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }

        let value: String = match chars.get(j) {
            Some(&quote @ ('"' | '\'')) => {
                j += 1;
                let start = j;
                while j < chars.len() && chars[j] != quote {
                    j += 1;
                }
                let value: String = chars[start..j].iter().collect();
                // An unclosed quote runs to the end of the tag, which is what
                // a browser does with it too.
                j = (j + 1).min(chars.len());
                value
            }
            _ => {
                let start = j;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                let value: String = chars[start..j].iter().collect();
                // `<img src=x/>`: the slash closes the tag, it is not part of
                // the address.
                value.trim_end_matches('/').to_string()
            }
        };

        out.push((name, Some(decode_entities(&value))));
        i = j;
    }

    out
}

/// Reads an attribute out of a raw tag body.
fn attr(raw: &str, name: &str) -> Option<String> {
    attributes(raw)
        .into_iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value)
}

/// One row as the splitter found it, with its cells still to be parsed.
struct RawRow {
    header: bool,
    cells: Vec<String>,
}

/// The same row once its cells have been parsed into blocks.
struct Row {
    header: bool,
    cells: Vec<Vec<Block>>,
}

/// Finds the end of the element opened at `start`, honouring nesting.
///
/// Returns the offset of the closing tag and the offset just past it. An
/// unclosed element — common enough in mail — runs to the end of the input,
/// which shows the content rather than losing it.
fn match_end(input: &[u8], start: usize, name: &[u8]) -> (usize, usize) {
    let mut pos = start;
    let mut depth = 1usize;

    while pos < input.len() {
        let Some(next) = input[pos..].iter().position(|&b| b == b'<') else { break };
        pos += next;
        let rest = &input[pos..];

        let closing = rest.starts_with(b"</");
        let after_bracket = if closing { 2 } else { 1 };
        let tail = &rest[after_bracket.min(rest.len())..];

        if tail.len() >= name.len() && tail[..name.len()].eq_ignore_ascii_case(name) {
            // Require a delimiter, so `<tablet>` is not a `<table>`.
            let delimited = tail
                .get(name.len())
                .is_none_or(|b| !b.is_ascii_alphanumeric());
            if delimited {
                let end = rest.iter().position(|&b| b == b'>').map_or(rest.len(), |i| i + 1);
                if closing {
                    depth -= 1;
                    if depth == 0 {
                        return (pos, pos + end);
                    }
                } else {
                    depth += 1;
                }
                pos += end;
                continue;
            }
        }
        pos += 1;
    }

    (input.len(), input.len())
}

/// Splits a table's inner HTML into rows and cells.
///
/// Only tags belonging to *this* table are considered: a nested table is
/// skipped over whole, so its `<tr>` and `<td>` do not tear the outer row
/// apart. Cells outside any `<tr>` are tolerated, because mail does that.
fn split_rows(html: &[u8]) -> Vec<RawRow> {
    let mut rows: Vec<RawRow> = Vec::new();
    let mut loose: Option<RawRow> = None;
    let mut pos = 0;

    while pos < html.len() {
        let Some(next) = html[pos..].iter().position(|&b| b == b'<') else { break };
        pos += next;
        let rest = &html[pos..];
        let Some(close) = rest.iter().position(|&b| b == b'>') else { break };
        let tag = &rest[1..close];
        let name: Vec<u8> = tag
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || **b == b'/')
            .copied()
            .collect();
        let lower = String::from_utf8_lossy(&name).to_ascii_lowercase();
        pos += close + 1;

        match lower.as_str() {
            // A nested table belongs to whichever cell contains it; step over
            // it so its rows are not mistaken for this table's.
            "table" => pos = match_end(html, pos, b"table").1,
            "tr" => rows.push(RawRow { header: false, cells: Vec::new() }),
            "td" | "th" => {
                let (inner, after) = match_end(html, pos, lower.as_bytes());
                let cell = String::from_utf8_lossy(&html[pos..inner]).into_owned();
                pos = after;

                let row = match rows.last_mut() {
                    Some(row) => row,
                    // Cells before any `<tr>`: collect them as one row rather
                    // than dropping them.
                    None => loose.get_or_insert(RawRow { header: false, cells: Vec::new() }),
                };
                row.header |= lower == "th";
                row.cells.push(cell);
            }
            _ => {}
        }
    }

    if let Some(loose) = loose.filter(|r| !r.cells.is_empty()) {
        rows.insert(0, loose);
    }
    rows.retain(|row| !row.cells.is_empty());
    rows
}

/// Whether a table carries data, as opposed to holding a layout together.
///
/// Mail is built almost entirely out of layout tables, and rendering those as
/// grids is what makes a newsletter look like a spreadsheet. The distinction
/// that actually holds up in practice:
///
/// * an explicit `<th>` means the sender meant a table;
/// * otherwise it needs at least two rows of the same width, at least two
///   columns wide — a receipt, not a wrapper;
/// * and its cells must hold text. A cell containing an image, a heading, a
///   nested table or several paragraphs is a panel in a layout, whatever its
///   shape.
fn is_data_table(rows: &[Row]) -> bool {
    if rows.is_empty() {
        return false;
    }

    // An image, a nested table, a heading or a rule is the mark of a panel in
    // a layout, whatever shape the thing holding it has.
    let only_text = rows.iter().flat_map(|r| &r.cells).all(|cell| {
        !cell.iter().any(|block| {
            matches!(block, Block::Image(_) | Block::Table(_) | Block::Heading { .. } | Block::Rule)
        })
    });
    if !only_text {
        return false;
    }

    // An explicit `<th>` is the sender saying "this is a table", and that is
    // worth more than any heuristic. A data cell may well hold two blocks —
    // a number and the delta under it, a total and its currency line — and
    // demoting the whole table for that turns a service dashboard into
    // twenty-four loose paragraphs.
    if rows.iter().any(|row| row.header) {
        return true;
    }

    // Without one, the shape has to carry it: two or more rows of equal
    // width, two or more columns, and one block per cell. Anything looser
    // matches the wrapper tables every newsletter is built out of.
    let one_block_per_cell = rows.iter().flat_map(|r| &r.cells).all(|cell| cell.len() <= 1);
    let width = rows[0].cells.len();
    one_block_per_cell
        && width >= 2
        && rows.len() >= 2
        && rows.iter().all(|row| row.cells.len() == width)
}

/// A layout row's cells sat side by side, so when each holds a single short
/// line they belong on one line here too.
///
/// Stacking them is the approximation that turns a band of stats into a
/// column, and a chart's day labels into seven paragraphs each saying one
/// letter. A cell holding more than one block, or a real paragraph of prose,
/// is a column of content — that one keeps its own line.
fn join_band(row: &Row) -> Option<Vec<Span>> {
    /// Longer than this and the row is prose in columns, not a band.
    const MAX_BAND: usize = 120;

    let mut spans: Vec<Span> = Vec::new();
    let mut cells = 0usize;
    let mut length = 0usize;

    // Every block boundary crossed here was a line break in the original, so
    // each one becomes the space that keeps `171.4K` off the end of
    // `Requests`.
    fn separate(spans: &mut [Span], length: &mut usize) {
        if let Some(last) = spans.last_mut()
            && !last.text.ends_with(' ')
        {
            last.text.push(' ');
            *length += 1;
        }
    }

    for cell in &row.cells {
        // An empty cell is a spacer: it contributes nothing but does not
        // disqualify the row. Anything that is not prose — an image, a nested
        // table, a heading — means this is a panel, not a band.
        if cell.is_empty() {
            continue;
        }
        if !cell.iter().all(|block| matches!(block, Block::Paragraph(_))) {
            return None;
        }

        cells += 1;
        for block in cell {
            separate(&mut spans, &mut length);
            for span in block.spans() {
                length += span.text.chars().count();
                if length > MAX_BAND {
                    return None;
                }
                match spans.last_mut() {
                    Some(last) if last.style == span.style && last.link == span.link => {
                        last.text.push_str(&span.text)
                    }
                    _ => spans.push(span.clone()),
                }
            }
        }
    }

    if cells < 2 {
        return None;
    }
    if let Some(last) = spans.last_mut() {
        last.text = last.text.trim_end().to_string();
    }
    spans.retain(|span| !span.text.is_empty());
    (!spans.is_empty()).then_some(spans)
}

/// Flattens parsed cells into a grid of spans.
fn to_table(rows: &[Row]) -> Table {
    let flatten = |cell: &Vec<Block>| -> Cell {
        let mut spans: Vec<Span> = Vec::new();
        for block in cell {
            // Blocks within one cell are separate lines in the original and
            // one line here, so they need the space between them that the
            // line break used to provide.
            if let Some(last) = spans.last_mut()
                && !last.text.ends_with(' ')
            {
                last.text.push(' ');
            }
            for span in block.spans() {
                match spans.last_mut() {
                    Some(last) if last.style == span.style && last.link == span.link => {
                        last.text.push_str(&span.text)
                    }
                    _ => spans.push(span.clone()),
                }
            }
        }
        spans
    };

    let mut headers = Vec::new();
    let mut body = Vec::new();
    for row in rows {
        let cells: Vec<Cell> = row.cells.iter().map(flatten).collect();
        // Only the first header row becomes the header; a table that repeats
        // its headers mid-way keeps the repeats as ordinary rows.
        if row.header && headers.is_empty() {
            headers = cells;
        } else {
            body.push(cells);
        }
    }

    Table { headers, rows: body }
}

/// Splits a `style` attribute into its declarations, property lowercased.
///
/// Not a CSS parser: a `url(data:…;base64,…)` value carries a semicolon and
/// will split in the wrong place. That costs a background image nobody was
/// going to see, and the alternative is a tokenizer for a language this
/// renderer reads six properties of.
fn declarations(style: &str) -> impl Iterator<Item = (String, String)> + '_ {
    style.split(';').filter_map(|declaration| {
        let (property, value) = declaration.split_once(':')?;
        let property = property.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        (!property.is_empty() && !value.is_empty()).then_some((property, value))
    })
}

/// Whether an element's box is drawn around words rather than around a region.
///
/// The distinction decides what a background means: behind a `<span>` it is a
/// badge, behind a `<div>` it is a card.
fn is_inline(name: &str) -> bool {
    matches!(
        name,
        "span" | "a" | "b" | "strong" | "i" | "em" | "u" | "ins" | "s" | "del" | "strike"
            | "code" | "tt" | "kbd" | "samp" | "mark" | "small" | "big" | "font" | "sub" | "sup"
            | "abbr" | "label" | "time"
    )
}

/// Whether a `font-weight` means bold. `600` is where mail's "semibold" sits.
fn is_bold(value: &str) -> bool {
    match value.trim().parse::<u16>() {
        Ok(weight) => weight >= 600,
        Err(_) => matches!(value.trim(), "bold" | "bolder"),
    }
}

/// Whether a `font-family` stack asks for a monospace face.
///
/// Matched on the families mail actually names rather than on the generic
/// keyword alone, because almost nobody writes `monospace` first.
fn is_monospace(value: &str) -> bool {
    ["monospace", "mono", "menlo", "consolas", "courier", "monaco", "roboto mono", "ui-monospace"]
        .iter()
        .any(|family| value.contains(family))
}

/// Reads a CSS colour: `#abc`, `#aabbcc`, `#aabbccdd`, `rgb()`, `rgba()`, or
/// one of the names mail actually uses.
///
/// `transparent` and anything unparseable yield `None`, which means "the
/// sender did not say", not "black".
fn parse_color(raw: &str) -> Option<Rgba> {
    let value = raw.trim().to_ascii_lowercase();

    if let Some(hex) = value.strip_prefix('#') {
        let digits: Vec<u8> = hex.chars().filter_map(|c| c.to_digit(16).map(|d| d as u8)).collect();
        if digits.len() != hex.len() {
            return None;
        }
        let pair = |hi: u8, lo: u8| hi * 16 + lo;
        return match digits.as_slice() {
            // `#abc` and `#abcd` are the shorthand where each digit doubles.
            [r, g, b] => Some(Rgba { r: pair(*r, *r), g: pair(*g, *g), b: pair(*b, *b), a: 255 }),
            [r, g, b, a] => Some(Rgba {
                r: pair(*r, *r),
                g: pair(*g, *g),
                b: pair(*b, *b),
                a: pair(*a, *a),
            }),
            [r1, r2, g1, g2, b1, b2] => {
                Some(Rgba { r: pair(*r1, *r2), g: pair(*g1, *g2), b: pair(*b1, *b2), a: 255 })
            }
            [r1, r2, g1, g2, b1, b2, a1, a2] => Some(Rgba {
                r: pair(*r1, *r2),
                g: pair(*g1, *g2),
                b: pair(*b1, *b2),
                a: pair(*a1, *a2),
            }),
            _ => None,
        };
    }

    if let Some(args) = value.strip_prefix("rgba(").or_else(|| value.strip_prefix("rgb(")) {
        let args = args.trim_end_matches(')');
        let parts: Vec<&str> = args.split([',', '/', ' ']).filter(|p| !p.trim().is_empty()).collect();
        let channel = |part: &str| -> Option<u8> {
            let part = part.trim();
            match part.strip_suffix('%') {
                Some(percent) => percent.parse::<f32>().ok().map(|p| (p / 100. * 255.) as u8),
                None => part.parse::<f32>().ok().map(|v| v.clamp(0., 255.) as u8),
            }
        };
        let (r, g, b) = (channel(parts.first()?)?, channel(parts.get(1)?)?, channel(parts.get(2)?)?);
        let a = match parts.get(3) {
            Some(alpha) => match alpha.trim().strip_suffix('%') {
                Some(percent) => (percent.parse::<f32>().ok()? / 100. * 255.) as u8,
                None => (alpha.trim().parse::<f32>().ok()?.clamp(0., 1.) * 255.) as u8,
            },
            None => 255,
        };
        return Some(Rgba { r, g, b, a });
    }

    let named: &[(&str, u32)] = &[
        ("black", 0x000000),
        ("white", 0xffffff),
        ("red", 0xff0000),
        ("green", 0x008000),
        ("blue", 0x0000ff),
        ("gray", 0x808080),
        ("grey", 0x808080),
        ("silver", 0xc0c0c0),
        ("navy", 0x000080),
        ("teal", 0x008080),
        ("orange", 0xffa500),
        ("yellow", 0xffff00),
        ("purple", 0x800080),
        ("maroon", 0x800000),
        ("olive", 0x808000),
        ("lime", 0x00ff00),
        ("aqua", 0x00ffff),
        ("cyan", 0x00ffff),
        ("fuchsia", 0xff00ff),
        ("magenta", 0xff00ff),
        ("pink", 0xffc0cb),
        ("brown", 0xa52a2a),
    ];
    named.iter().find(|(name, _)| *name == value).map(|(_, hex)| Rgba {
        r: (hex >> 16) as u8,
        g: ((hex >> 8) & 0xff) as u8,
        b: (hex & 0xff) as u8,
        a: 255,
    })
}

/// Elements with no closing tag, which therefore have no subtree to skip.
fn is_void(name: &str) -> bool {
    matches!(
        name,
        "br" | "hr" | "img" | "input" | "meta" | "link" | "area" | "base" | "col"
            | "embed" | "source" | "track" | "wbr"
    )
}

/// Whether a tag's attributes say the sender did not want this shown.
///
/// Deliberately only the declarations that mean "hidden" and nothing else.
/// `font-size:0`, `max-height:0` and `opacity:0` all *also* appear on ordinary
/// wrapper elements in marketing HTML, where they control whitespace rather
/// than visibility — treating those as hidden blanks the whole message, which
/// is exactly what happened the first time this was written. A preheader is
/// always `display:none` anyway; the rest are belt and braces on top of it.
///
/// This is not a CSS engine and does not need to be: it looks for the handful
/// of ways mail says "hide this", not a cascade.
fn is_hidden(raw: &str) -> bool {
    if has_flag(raw, "hidden") {
        return true;
    }

    let Some(style) = attr(raw, "style") else { return false };
    // Whitespace between property and value varies; comparing without it
    // means one pattern per declaration instead of three.
    let style: String = style
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();

    ["display:none", "visibility:hidden", "mso-hide:all"]
        .iter()
        .any(|pattern| style.contains(pattern))
}

/// Whether a valueless attribute like `hidden` is present.
///
/// Read off the parsed attributes rather than searched for, because `hidden`
/// is a word that turns up in CSS: `overflow: hidden` on a rounded-corner card
/// is in most marketing mail, and reading it as the attribute hid the entire
/// message.
fn has_flag(raw: &str, name: &str) -> bool {
    attributes(raw).into_iter().any(|(key, value)| {
        key.eq_ignore_ascii_case(name)
            // `hidden="false"` means what it says.
            && !value.is_some_and(|value| value.eq_ignore_ascii_case("false"))
    })
}

/// Reads an HTML length attribute: `120`, `120px`, or a percentage.
///
/// A percentage yields `None` — it is relative to a container this renderer
/// does not have, so "unknown" is more honest than a number.
fn parse_dimension(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    if raw.ends_with('%') {
        return None;
    }
    let digits: String = raw.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Whether declared dimensions mark this as a spacer or a tracking pixel.
fn is_spacer(width: Option<u32>, height: Option<u32>) -> bool {
    // Either dimension at a couple of pixels means it cannot be an image
    // anyone was meant to look at.
    matches!(width, Some(w) if w <= 2) || matches!(height, Some(h) if h <= 2)
}

/// Classifies an `<img src>`.
///
/// Anything not recognised — `about:`, a bare filename, a `javascript:` URL —
/// yields `None`, and the image falls back to its alt text.
fn parse_image_source(src: &str) -> Option<ImageSource> {
    let src = src.trim();
    if src.is_empty() {
        return None;
    }
    let lower = src.to_ascii_lowercase();

    if let Some(id) = lower.strip_prefix("cid:") {
        let id = src[src.len() - id.len()..].trim();
        // Content-IDs are written with angle brackets in the header and
        // without them in the URL; strip either so the two forms compare.
        let id = id.trim_start_matches('<').trim_end_matches('>');
        return (!id.is_empty()).then(|| ImageSource::Cid(id.to_string()));
    }

    if lower.starts_with("data:") {
        let body = &src["data:".len()..];
        let (meta, payload) = body.split_once(',')?;
        // Only base64 payloads are worth carrying; a percent-encoded data URL
        // holding an image is vanishingly rare.
        if !meta.to_ascii_lowercase().contains("base64") {
            return None;
        }
        let mime = meta.split(';').next().unwrap_or("image/png").trim();
        let mime = if mime.is_empty() { "image/png" } else { mime };
        return Some(ImageSource::Data {
            mime: mime.to_string(),
            base64: payload.trim().to_string(),
        });
    }

    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(ImageSource::Remote(src.to_string()));
    }
    // Protocol-relative, which mail still uses. HTTPS is the safe reading.
    if let Some(rest) = src.strip_prefix("//") {
        return Some(ImageSource::Remote(format!("https://{rest}")));
    }

    None
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_sub_ci(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

/// The named entities that turn up in mail, sorted for binary search.
///
/// Not the full HTML5 set — that is some two thousand names, most of which no
/// mail has ever contained. This is the long tail that actually appears,
/// including the invisible ones marketing mail pads its preview text with.
/// Numeric references are handled separately and need no table.
const ENTITIES: &[(&str, char)] = &[
    ("AElig", '\u{c6}'), ("Aacute", '\u{c1}'), ("Acirc", '\u{c2}'), ("Agrave", '\u{c0}'),
    ("Aring", '\u{c5}'), ("Atilde", '\u{c3}'), ("Auml", '\u{c4}'), ("Ccedil", '\u{c7}'),
    ("Dagger", '\u{2021}'), ("ETH", '\u{d0}'), ("Eacute", '\u{c9}'), ("Ecirc", '\u{ca}'),
    ("Egrave", '\u{c8}'), ("Euml", '\u{cb}'), ("Iacute", '\u{cd}'), ("Icirc", '\u{ce}'),
    ("Igrave", '\u{cc}'), ("Iuml", '\u{cf}'), ("Ntilde", '\u{d1}'), ("Oacute", '\u{d3}'),
    ("Ocirc", '\u{d4}'), ("Ograve", '\u{d2}'), ("Oslash", '\u{d8}'), ("Otilde", '\u{d5}'),
    ("Ouml", '\u{d6}'), ("Prime", '\u{2033}'), ("THORN", '\u{de}'), ("Uacute", '\u{da}'),
    ("Ucirc", '\u{db}'), ("Ugrave", '\u{d9}'), ("Uuml", '\u{dc}'), ("Yacute", '\u{dd}'),
    ("aacute", '\u{e1}'), ("acirc", '\u{e2}'), ("acute", '\u{b4}'), ("aelig", '\u{e6}'),
    ("agrave", '\u{e0}'), ("amp", '&'), ("apos", '\''), ("aring", '\u{e5}'),
    ("asymp", '\u{2248}'), ("atilde", '\u{e3}'), ("auml", '\u{e4}'), ("bdquo", '\u{201e}'),
    ("brvbar", '\u{a6}'), ("bull", '\u{2022}'), ("ccedil", '\u{e7}'), ("cedil", '\u{b8}'),
    ("cent", '\u{a2}'), ("check", '\u{2713}'), ("clubs", '\u{2663}'), ("copy", '\u{a9}'),
    ("curren", '\u{a4}'), ("dagger", '\u{2020}'), ("darr", '\u{2193}'), ("deg", '\u{b0}'),
    ("diams", '\u{2666}'), ("divide", '\u{f7}'), ("eacute", '\u{e9}'), ("ecirc", '\u{ea}'),
    ("egrave", '\u{e8}'), ("emsp", '\u{2003}'), ("ensp", '\u{2002}'), ("eth", '\u{f0}'),
    ("euml", '\u{eb}'), ("euro", '\u{20ac}'), ("frac12", '\u{bd}'), ("frac14", '\u{bc}'),
    ("frac34", '\u{be}'), ("ge", '\u{2265}'), ("gt", '>'), ("harr", '\u{2194}'),
    ("hearts", '\u{2665}'), ("hellip", '\u{2026}'), ("iacute", '\u{ed}'), ("icirc", '\u{ee}'),
    ("iexcl", '\u{a1}'), ("igrave", '\u{ec}'), ("infin", '\u{221e}'), ("iquest", '\u{bf}'),
    ("iuml", '\u{ef}'), ("laquo", '\u{ab}'), ("larr", '\u{2190}'), ("ldquo", '\u{201c}'),
    ("le", '\u{2264}'), ("loz", '\u{25ca}'), ("lrm", '\u{200e}'), ("lsaquo", '\u{2039}'),
    ("lsquo", '\u{2018}'), ("lt", '<'), ("macr", '\u{af}'), ("mdash", '\u{2014}'),
    ("micro", '\u{b5}'), ("middot", '\u{b7}'), ("minus", '\u{2212}'), ("nbsp", ' '),
    ("ndash", '\u{2013}'), ("ne", '\u{2260}'), ("not", '\u{ac}'), ("ntilde", '\u{f1}'),
    ("oacute", '\u{f3}'), ("ocirc", '\u{f4}'), ("ograve", '\u{f2}'), ("ordf", '\u{aa}'),
    ("ordm", '\u{ba}'), ("oslash", '\u{f8}'), ("otilde", '\u{f5}'), ("ouml", '\u{f6}'),
    ("para", '\u{b6}'), ("permil", '\u{2030}'), ("plusmn", '\u{b1}'), ("pound", '\u{a3}'),
    ("prime", '\u{2032}'), ("quot", '"'), ("raquo", '\u{bb}'), ("rarr", '\u{2192}'),
    ("rdquo", '\u{201d}'), ("reg", '\u{ae}'), ("rlm", '\u{200f}'), ("rsaquo", '\u{203a}'),
    ("rsquo", '\u{2019}'), ("sbquo", '\u{201a}'), ("sect", '\u{a7}'),
    // The padding entities. Invisible by definition, and the reason a
    // newsletter can arrive with five thousand of them after its first line.
    ("shy", '\u{ad}'),
    ("spades", '\u{2660}'), ("star", '\u{2606}'), ("sup1", '\u{b9}'), ("sup2", '\u{b2}'),
    ("sup3", '\u{b3}'), ("szlig", '\u{df}'), ("thinsp", '\u{2009}'), ("thorn", '\u{fe}'),
    ("tilde", '\u{2dc}'), ("times", '\u{d7}'), ("trade", '\u{2122}'), ("uacute", '\u{fa}'),
    ("uarr", '\u{2191}'), ("ucirc", '\u{fb}'), ("ugrave", '\u{f9}'), ("uml", '\u{a8}'),
    ("uuml", '\u{fc}'), ("yacute", '\u{fd}'), ("yen", '\u{a5}'), ("yuml", '\u{ff}'),
    ("zwj", '\u{200d}'), ("zwnj", '\u{200c}'),
];

/// Decodes the named entities that turn up in mail, plus numeric ones.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];

        // Long enough for `&thinsp;` and every numeric reference, short enough
        // that a stray `&` in prose does not scan half the paragraph.
        let Some(semi) = after.find(';').filter(|ix| *ix <= 8) else {
            out.push('&');
            rest = after;
            continue;
        };

        let name = &after[..semi];
        let decoded = match name.strip_prefix('#') {
            Some(digits) => match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok(),
                None => digits.parse().ok(),
            }
            .and_then(char::from_u32),
            None => ENTITIES
                .binary_search_by_key(&name, |(entity, _)| *entity)
                .ok()
                .map(|ix| ENTITIES[ix].1),
        };

        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Characters that occupy no space and carry no meaning on their own.
///
/// Mail uses runs of these — soft hyphens, zero-width spaces, combining
/// grapheme joiners — to pad the preview line so the client shows the sender's
/// chosen teaser instead of the start of the body. Left in, they are thousands
/// of characters of nothing to shape.
fn is_invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{ad}'      // soft hyphen
        | '\u{34f}'   // combining grapheme joiner
        | '\u{200b}'  // zero-width space
        | '\u{200e}' | '\u{200f}' // directional marks
        | '\u{2060}'  // word joiner
        | '\u{feff}'  // zero-width no-break space
    )
}

/// Zero-width joiners, which *do* carry meaning between two visible
/// characters: `👨\u{200d}👩` is one emoji, and Persian and Indic text needs
/// them inside words. Dropped only where they are padding.
fn is_joiner(ch: char) -> bool {
    matches!(ch, '\u{200c}' | '\u{200d}')
}

/// True when a string has nothing a reader could see.
pub fn is_blank(text: &str) -> bool {
    text.chars().all(|ch| ch.is_whitespace() || is_invisible(ch) || is_joiner(ch))
}

/// Builds the one-line preview shown in the message list.
pub fn snippet(plain: &str, html: &str, max_chars: usize) -> String {
    let source: String = if plain.trim().is_empty() {
        html_to_blocks(html)
            .iter()
            .filter(|b| !matches!(b, Block::Quote { .. }))
            .map(Block::text)
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        // Skip quoted lines: a reply's preview should show the reply.
        plain
            .lines()
            .filter(|l| !l.trim_start().starts_with('>'))
            .collect::<Vec<_>>()
            .join(" ")
    };

    let collapsed = collapse_ws(&source);
    let trimmed = collapsed.trim();

    match trimmed.char_indices().nth(max_chars) {
        Some((ix, _)) => format!("{}\u{2026}", trimmed[..ix].trim_end()),
        None => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(blocks: &[Block]) -> String {
        blocks
            .iter()
            .map(|b| match b {
                Block::Rule => "---".into(),
                other => other.text(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn script_and_style_bodies_are_dropped() {
        let blocks = html_to_blocks(
            "<style>p { color: red }</style><p>Hello</p><script>alert('x')</script>",
        );
        assert_eq!(text_of(&blocks), "Hello");
    }

    #[test]
    fn inline_styles_become_spans() {
        let blocks = html_to_blocks("<p>plain <b>bold</b> <i>it</i></p>");
        let spans = blocks[0].spans();
        assert!(spans.iter().any(|s| s.text.contains("bold") && s.style.bold));
        assert!(spans.iter().any(|s| s.text.contains("it") && s.style.italic));
    }

    #[test]
    fn links_keep_their_href() {
        let blocks = html_to_blocks(r#"<p>see <a href="https://example.com">this</a></p>"#);
        let link = blocks[0].spans().iter().find(|s| s.text == "this").unwrap();
        assert_eq!(link.link.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn javascript_hrefs_are_refused() {
        let blocks = html_to_blocks(r#"<a href="javascript:steal()">click</a>"#);
        assert!(blocks[0].spans().iter().all(|s| s.link.is_none()));
    }

    #[test]
    fn blockquotes_nest() {
        let blocks = html_to_blocks("<blockquote><blockquote><p>deep</p></blockquote></blockquote>");
        assert!(matches!(blocks[0], Block::Quote { depth: 2, .. }));
    }

    #[test]
    fn lists_number_themselves() {
        let blocks = html_to_blocks("<ol><li>one</li><li>two</li></ol>");
        match &blocks[1] {
            Block::ListItem { ordered, index, .. } => {
                assert!(ordered);
                assert_eq!(*index, 2);
            }
            other => panic!("expected a list item, got {other:?}"),
        }
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(decode_entities("a &amp; b &#65; &#x42; &nope;"), "a & b A B &nope;");
    }

    #[test]
    fn malformed_markup_still_renders() {
        let blocks = html_to_blocks("<p>unclosed <b>bold <div>next 5 < 6");
        assert!(text_of(&blocks).contains("unclosed"));
        assert!(text_of(&blocks).contains("6"));
    }

    #[test]
    fn plain_text_quoting_becomes_quote_blocks() {
        let blocks = plain_to_blocks("Hi\n\n> they said\n>> earlier still");
        assert!(matches!(blocks[0], Block::Paragraph(_)));
        assert!(matches!(blocks[1], Block::Quote { depth: 1, .. }));
        assert!(matches!(blocks[2], Block::Quote { depth: 2, .. }));
    }

    #[test]
    fn bare_urls_in_plain_text_are_linkified() {
        let blocks = plain_to_blocks("go to https://example.com/x, then stop");
        let link = blocks[0].spans().iter().find(|s| s.link.is_some()).unwrap();
        // The trailing comma is punctuation, not part of the URL.
        assert_eq!(link.link.as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn snippets_skip_quoted_text_and_truncate() {
        let s = snippet("> old thread\nMy actual reply here", "", 10);
        assert_eq!(s, "My actual\u{2026}");
    }

    #[test]
    fn a_table_with_headers_stays_a_table_when_a_cell_holds_a_value_and_a_delta() {
        // The shape every service dashboard mails out. Demoting it to layout
        // because one cell held two paragraphs turned a four-column table
        // into twenty loose lines.
        let html = r#"<table>
            <thead><tr><th>Service</th><th>Reqs</th></tr></thead>
            <tbody>
              <tr><td>web</td><td><p>161.4K</p><p>up 118.7%</p></td></tr>
              <tr><td>api</td><td><p>536</p><p>up 441.4%</p></td></tr>
            </tbody>
          </table>"#;
        let blocks = html_to_blocks(html);

        let Some(Block::Table(table)) = blocks.first() else {
            panic!("expected a table, got {blocks:?}");
        };
        assert_eq!(table.headers.len(), 2);
        assert_eq!(table.rows.len(), 2);
        // The two blocks in one cell read as one line, with the space the
        // line break used to provide.
        let cell: String = table.rows[0][1].iter().map(|s| s.text.as_str()).collect();
        assert_eq!(cell, "161.4K up 118.7%");
    }

    #[test]
    fn a_chart_drawn_in_empty_table_cells_does_not_become_blank_rows() {
        // Bar charts in mail are table cells with a height and a background
        // and no content. Rendering those as grid lines claims the sender
        // sent an empty row.
        let html = r#"<table>
            <tr>
              <td style="height:52px"><div style="height:24px;background:#e8872a"></div></td>
              <td style="height:52px"><div style="height:25px;background:#e8872a"></div></td>
            </tr>
            <tr><td>M</td><td>T</td></tr>
          </table>"#;
        let blocks = html_to_blocks(html);

        assert!(
            !blocks.iter().any(|b| matches!(b, Block::Table(_))),
            "one row of labels is not a table: {blocks:?}",
        );
        assert_eq!(text_of(&blocks), "M T");
    }

    #[test]
    fn a_row_of_short_cells_reads_as_one_line() {
        // The cells sat side by side. Stacking them turns a band of stats
        // into a column, and a chart's day labels into one paragraph per
        // letter.
        let html = "<table><tr><td>Requests</td><td>171.4K</td><td>up 89.1%</td></tr></table>";
        assert_eq!(text_of(&html_to_blocks(html)), "Requests 171.4K up 89.1%");
    }

    #[test]
    fn a_row_of_prose_columns_keeps_its_columns_apart() {
        // The other half of the rule: two columns of writing are a layout, and
        // running them together would produce one unreadable line.
        let long = "There is quite a lot to say about this, enough that it is \
                    plainly a column of prose rather than a stat in a band.";
        let html = format!("<table><tr><td>{long}</td><td>{long}</td></tr></table>");
        let blocks = html_to_blocks(&html);
        assert_eq!(blocks.len(), 2, "each column keeps its own block: {blocks:?}");
    }

    #[test]
    fn a_cell_holding_a_picture_is_a_panel_not_a_band() {
        let html = r#"<table><tr>
            <td><img src="cid:hero" width="600" alt="hero"></td>
            <td>Caption</td>
          </tr></table>"#;
        let blocks = html_to_blocks(html);
        assert!(blocks.iter().any(|b| matches!(b, Block::Image(_))));
        assert_eq!(blocks.len(), 2, "the image keeps its own block: {blocks:?}");
    }

    #[test]
    fn a_wrapper_around_a_line_of_text_is_not_a_document() {
        // All of these arrive as the "HTML part" of a message that has nothing
        // to lay out. Rendering them as documents puts a white page in front
        // of one line.
        for html in [
            "",
            "   ",
            "<html><body></body></html>",
            "<html><body>Attached is the JPG</body></html>",
            "<html><body>there<br/></body></html>",
            "Attached is your receipt from TERMINIX",
            "<div><p>Thanks!</p></div>",
        ] {
            assert!(!is_document(html), "{html:?} should not count as a document");
        }
    }

    #[test]
    fn anything_with_layout_in_it_is_a_document() {
        for html in [
            "<table><tr><td>Total</td><td>$4</td></tr></table>",
            "<img src=\"cid:hero\">",
            "<a href=\"https://example.test\">Open</a>",
            "<h1>Weekly digest</h1>",
            "<ul><li>one</li></ul>",
            "<body><style>p{color:red}</style><p>hi</p></body>",
        ] {
            assert!(is_document(html), "{html:?} should count as a document");
        }
    }

    #[test]
    fn a_message_whose_html_says_nothing_is_read_as_plain_text() {
        // Which is what puts it in the reader's own colours rather than on a
        // rendered page.
        let plain = "Attached is the JPG";
        let html = "<html><body>Attached is the JPG</body></html>";
        assert_eq!(effective_source(plain, html, Source::Rich), Source::Plain);
        assert!(!has_both(plain, html), "there is nothing to switch between");

        // A real one still wins.
        let rich = "<table><tr><td>Attached is the JPG</td></tr></table>";
        assert_eq!(effective_source(plain, rich, Source::Rich), Source::Rich);
        assert!(has_both(plain, rich));
    }

    #[test]
    fn a_message_with_only_a_bare_html_part_still_renders() {
        // No plain body at all: the line in the HTML part is the message, and
        // an empty reader is never the right answer.
        let blocks = body_to_blocks("", "<html><body>there</body></html>", Source::Rich);
        assert_eq!(text_of(&blocks), "there");
    }

    #[test]
    fn overflow_hidden_is_css_not_the_hidden_attribute() {
        // The rounded-corner card every transactional email is built out of.
        // Reading the word "hidden" out of the style blanked the whole
        // message, because the element carrying it wraps all of the content.
        let html = r#"<table style="border-radius: .75rem; overflow: hidden; background: #1e1b18;">
            <tr><td><p>the entire message</p></td></tr>
          </table>"#;
        assert_eq!(text_of(&html_to_blocks(html)), "the entire message");

        for style in ["overflow:hidden", "overflow-x: hidden", "text-overflow: hidden"] {
            let html = format!("<div style=\"{style}\">shown</div>");
            assert_eq!(text_of(&html_to_blocks(&html)), "shown", "style={style:?}");
        }
    }

    #[test]
    fn a_word_inside_an_attribute_value_is_not_an_attribute() {
        // The general form of the bug above: attribute values contain
        // everything a tag does, so a name has to be read off the parsed
        // attributes rather than found in the text.
        assert_eq!(attr(r#"img data-src="lazy.png" src="real.png""#, "src").as_deref(), Some("real.png"));
        assert_eq!(attr(r#"div class="hidden-until-open" style="color:red""#, "style").as_deref(), Some("color:red"));
        assert!(!has_flag(r#"div class="not-hidden""#, "hidden"));
        assert!(!has_flag(r#"div title="hidden costs""#, "hidden"));
        assert!(has_flag(r#"div hidden class="x""#, "hidden"));
        assert!(!has_flag(r#"div hidden="false""#, "hidden"));
    }

    #[test]
    fn attributes_survive_the_punctuation_that_turns_up_in_values() {
        let tag = r#"a href="https://x.test/a?b=1&amp;c=2" title="1 > 0" data-x='single' hidden"#;
        let found = attributes(tag);

        assert_eq!(attr(tag, "href").as_deref(), Some("https://x.test/a?b=1&c=2"));
        assert_eq!(attr(tag, "title").as_deref(), Some("1 > 0"));
        assert_eq!(attr(tag, "data-x").as_deref(), Some("single"));
        assert!(found.iter().any(|(name, value)| name == "hidden" && value.is_none()));
        // The element name is not an attribute.
        assert!(!found.iter().any(|(name, _)| name == "a"));
    }

    #[test]
    fn an_unquoted_value_stops_at_the_self_closing_slash() {
        assert_eq!(attr("img src=logo.png/", "src").as_deref(), Some("logo.png"));
        assert_eq!(attr("img src=logo.png width=32", "width").as_deref(), Some("32"));
    }

    #[test]
    fn a_hidden_preheader_is_not_rendered_twice() {
        // The shape every newsletter starts with: the teaser the inbox list
        // shows, hidden from the message body itself.
        let html = r#"
            <div style="display:none;max-height:0;overflow:hidden">
              Please accept the appreciation.
            </div>
            <p>Please accept the appreciation.</p>"#;
        assert_eq!(text_of(&html_to_blocks(html)), "Please accept the appreciation.");
    }

    #[test]
    fn the_ways_mail_hides_things_are_all_recognized() {
        for style in [
            "display:none",
            "display: none;",
            "DISPLAY:NONE",
            "visibility:hidden",
            "mso-hide:all",
            "color:#fff;display:none;max-height:0",
        ] {
            let html = format!("<div style=\"{style}\">secret</div><p>shown</p>");
            assert_eq!(text_of(&html_to_blocks(&html)), "shown", "style={style:?}");
        }

        assert_eq!(text_of(&html_to_blocks("<div hidden>secret</div><p>shown</p>")), "shown");
    }

    #[test]
    fn a_visible_element_is_not_mistaken_for_a_hidden_one() {
        // The check must not fire on ordinary styling, or mail loses its
        // body — which is precisely what a broader rule did to a real
        // newsletter whose wrapper carried `font-size:0` for whitespace
        // control.
        for style in [
            "display:block",
            "font-size:14px",
            "font-size:0",
            "opacity:0.9",
            "opacity:0",
            "max-height:0",
            "max-height:400px",
        ] {
            let html = format!("<div style=\"{style}\">shown</div>");
            assert_eq!(text_of(&html_to_blocks(&html)), "shown", "style={style:?}");
        }
    }

    #[test]
    fn hiding_a_container_hides_everything_inside_it() {
        let html = r#"<div style="display:none">
            <p>gone</p><table><tr><td>also gone</td></tr></table>
            <img src="cid:x" width="90" alt="gone too">
          </div><p>kept</p>"#;

        let blocks = html_to_blocks(html);
        assert_eq!(text_of(&blocks), "kept");
        assert!(!blocks.iter().any(|b| matches!(b, Block::Image(_))));
    }

    #[test]
    fn a_void_element_with_a_hidden_style_swallows_nothing_after_it() {
        // `<img>` has no closing tag; treating it like a container would skip
        // the rest of the message looking for `</img>`.
        let html = r#"<img src="cid:a" style="display:none" width="90"><p>kept</p>"#;
        assert_eq!(text_of(&html_to_blocks(html)), "kept");
    }

    #[test]
    fn the_padding_entities_marketing_mail_uses_are_decoded_and_dropped() {
        // The bug this fixes: a newsletter whose preheader is padded with
        // thousands of these rendered the literal text "&shy; &zwnj; &shy;…"
        // as the body of the message.
        let html = format!(
            "<p>Please accept the appreciation. \u{2764}\u{fe0f} {}</p>",
            "&shy; &zwnj; ".repeat(500)
        );
        let text = text_of(&html_to_blocks(&html));

        assert!(!text.contains("&shy;"), "entity left undecoded");
        assert!(!text.contains("&zwnj;"), "entity left undecoded");
        assert!(text.starts_with("Please accept the appreciation."));
        assert!(text.len() < 80, "the padding is gone, not merely invisible: {text:?}");
    }

    #[test]
    fn a_block_of_nothing_but_padding_produces_no_block_at_all() {
        assert!(html_to_blocks("<p>&shy;&zwnj;&#847;&nbsp;</p>").is_empty());
        assert!(html_to_blocks("<p>\u{200b}\u{feff}\u{2060}</p>").is_empty());
    }

    #[test]
    fn joiners_that_do_work_are_kept() {
        // A ZWJ between two visible characters is what makes one emoji out of
        // three, and Persian and Indic words need ZWNJ inside them.
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        let text = text_of(&html_to_blocks(&format!("<p>{family}</p>")));
        assert_eq!(text, family, "the family emoji must survive");

        let persian = "\u{645}\u{6cc}\u{200c}\u{62e}\u{648}\u{627}\u{647}\u{645}";
        let text = text_of(&html_to_blocks(&format!("<p>{persian}</p>")));
        assert_eq!(text, persian, "a ZWNJ inside a word is not padding");
    }

    #[test]
    fn the_entity_table_is_sorted_and_covers_what_mail_sends() {
        // Sorted because the lookup is a binary search; an out-of-order entry
        // would silently stop resolving.
        let mut sorted = ENTITIES.to_vec();
        sorted.sort_by_key(|(name, _)| *name);
        assert_eq!(sorted, ENTITIES, "ENTITIES must stay sorted by name");

        let decode_one = |name: &str| decode_entities(&format!("&{name};"));
        for (name, expected) in [
            ("shy", "\u{ad}"),
            ("zwnj", "\u{200c}"),
            ("eacute", "\u{e9}"),
            ("rsquo", "\u{2019}"),
            ("mdash", "\u{2014}"),
            ("times", "\u{d7}"),
            ("larr", "\u{2190}"),
            ("frac12", "\u{bd}"),
            ("check", "\u{2713}"),
        ] {
            assert_eq!(decode_one(name), expected, "&{name};");
        }
    }

    #[test]
    fn an_unknown_entity_is_left_alone_rather_than_eaten() {
        assert_eq!(decode_entities("&notareal;"), "&notareal;");
        assert_eq!(decode_entities("Tom & Jerry"), "Tom & Jerry");
        assert_eq!(decode_entities("100&nbsp;&#37;"), "100 %");
    }

    #[test]
    fn the_html_body_is_what_gets_rendered() {
        // The plain part is a lossy transcription, so it is not what you see
        // by default — as long as the HTML part is a document at all. A bare
        // `<p>` is not; see `is_document`.
        let rich = "<h1>from html</h1>";

        let blocks = body_to_blocks("from plain", rich, Source::Rich);
        assert_eq!(text_of(&blocks), "from html");

        let blocks = body_to_blocks("from plain", rich, Source::Plain);
        assert_eq!(text_of(&blocks), "from plain");

        assert!(has_both("from plain", rich));
        assert!(!has_both("", rich));
        assert_eq!(Source::Rich.other(), Source::Plain);
    }

    #[test]
    fn a_missing_part_falls_back_instead_of_rendering_nothing() {
        // Asking for a body the message does not have must never produce an
        // empty reader.
        assert_eq!(text_of(&body_to_blocks("only plain", "", Source::Rich)), "only plain");
        assert_eq!(
            text_of(&body_to_blocks("", "<p>only html</p>", Source::Plain)),
            "only html"
        );
        assert!(body_to_blocks("", "", Source::Rich).is_empty());
        assert!(body_to_blocks("   ", "  \n ", Source::Plain).is_empty());
    }

    #[test]
    fn the_reported_source_is_the_one_actually_used() {
        // The reader labels a message "plain text"; saying so about a message
        // rendered from its HTML because it had no plain part would be a lie.
        assert_eq!(effective_source("p", "<h1>h</h1>", Source::Rich), Source::Rich);
        assert_eq!(effective_source("p", "<h1>h</h1>", Source::Plain), Source::Plain);

        assert_eq!(effective_source("only plain", "", Source::Rich), Source::Plain);
        assert_eq!(effective_source("", "<p>only html</p>", Source::Plain), Source::Rich);
    }

    // ------------------------------------------------------------ images

    fn images(html: &str) -> Vec<Image> {
        html_to_blocks(html)
            .into_iter()
            .filter_map(|b| match b {
                Block::Image(image) => Some(image),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_attached_image_becomes_a_cid_block() {
        let found = images(r#"<p>hi</p><img src="cid:logo@example.com" alt="Logo">"#);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, ImageSource::Cid("logo@example.com".into()));
        assert_eq!(found[0].alt, "Logo");
    }

    #[test]
    fn a_content_id_matches_whether_or_not_it_is_bracketed() {
        // The header writes `<id>`; the URL usually does not. Both have to
        // resolve to the same key or inline images never appear.
        assert_eq!(
            images(r#"<img src="cid:<a@b>">"#)[0].source,
            ImageSource::Cid("a@b".into())
        );
    }

    #[test]
    fn remote_images_are_recognized_including_protocol_relative_ones() {
        assert_eq!(
            images(r#"<img src="https://example.com/a.png">"#)[0].source,
            ImageSource::Remote("https://example.com/a.png".into())
        );
        assert_eq!(
            images(r#"<img src="//cdn.example.com/a.png">"#)[0].source,
            ImageSource::Remote("https://cdn.example.com/a.png".into()),
            "a protocol-relative URL resolves to https, not http"
        );
    }

    #[test]
    fn a_data_url_keeps_its_payload_and_type() {
        let found = images(r#"<img src="data:image/gif;base64,R0lGOD">"#);
        assert_eq!(
            found[0].source,
            ImageSource::Data { mime: "image/gif".into(), base64: "R0lGOD".into() }
        );
        // Not base64 — nothing useful to carry.
        assert!(images(r#"<img src="data:image/svg+xml,<svg/>">"#).is_empty());
    }

    #[test]
    fn tracking_pixels_and_spacers_are_dropped() {
        // The single most common image in mail, and the one thing that must
        // never draw a box in the reader.
        assert!(images(r#"<img src="https://t.example.com/o.gif" width="1" height="1">"#).is_empty());
        assert!(images(r#"<img src="cid:s" width="600" height="1">"#).is_empty());
        assert!(images(r#"<img src="cid:s" width="2">"#).is_empty());
        // A real image with only one dimension declared survives.
        assert_eq!(images(r#"<img src="cid:s" width="600">"#).len(), 1);
    }

    #[test]
    fn declared_dimensions_are_kept_and_percentages_are_not() {
        let found = images(r#"<img src="cid:a" width="600px" height="240">"#);
        assert_eq!((found[0].width, found[0].height), (Some(600), Some(240)));

        // A percentage is relative to a container this renderer does not
        // have, so it is better read as "unknown".
        let found = images(r#"<img src="cid:a" width="100%">"#);
        assert_eq!(found[0].width, None);
    }

    #[test]
    fn an_unusable_source_falls_back_to_alt_text() {
        let blocks = html_to_blocks(r#"<p>a <img src="javascript:evil()" alt="fallback"> b</p>"#);
        assert!(text_of(&blocks).contains("[fallback]"));
        assert!(images(r#"<img src="javascript:evil()" alt="fallback">"#).is_empty());
        // Nothing at all to show: no block, no stray brackets.
        assert!(html_to_blocks(r#"<img src="about:blank">"#).is_empty());
    }

    // ------------------------------------------------------------ tables

    fn tables(html: &str) -> Vec<Table> {
        html_to_blocks(html)
            .into_iter()
            .filter_map(|b| match b {
                Block::Table(table) => Some(table),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_layout_table_is_unwrapped_into_its_contents() {
        // The shape every newsletter is built from. Rendering it as a grid is
        // what makes mail look like a spreadsheet.
        let html = r#"<table><tr><td><h1>Sale</h1><p>Ends Friday.</p></td></tr></table>"#;
        assert!(tables(html).is_empty(), "a one-cell table is scaffolding");

        let blocks = html_to_blocks(html);
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert_eq!(text_of(&blocks), "Sale\nEnds Friday.");
    }

    #[test]
    fn a_table_of_data_becomes_a_grid() {
        let html = r#"<table>
            <tr><th>Item</th><th>Price</th></tr>
            <tr><td>Widget</td><td>$20.00</td></tr>
            <tr><td>Shipping</td><td>$2.00</td></tr>
        </table>"#;

        let table = &tables(html)[0];
        assert_eq!(table.columns(), 2);
        assert_eq!(table.headers.len(), 2);
        assert_eq!(table.headers[0][0].text, "Item");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[1][0][0].text, "Shipping");
    }

    #[test]
    fn a_headerless_table_still_counts_as_data_when_it_is_shaped_like_data() {
        let html = r#"<table>
            <tr><td>Order</td><td>#4821</td></tr>
            <tr><td>Total</td><td>$42.00</td></tr>
        </table>"#;
        let table = &tables(html)[0];
        assert!(table.headers.is_empty());
        assert_eq!(table.rows.len(), 2);
    }

    #[test]
    fn a_two_column_layout_of_panels_is_not_mistaken_for_data() {
        // Same shape as the receipt above — two consistent columns — but the
        // cells hold images and headings, so it is a layout.
        let html = r#"<table>
            <tr><td><img src="cid:a" width="80" height="80"></td><td><h2>One</h2></td></tr>
            <tr><td><img src="cid:b" width="80" height="80"></td><td><h2>Two</h2></td></tr>
        </table>"#;
        assert!(tables(html).is_empty());
        assert_eq!(images(html).len(), 2, "the images survive the unwrapping");
    }

    #[test]
    fn a_data_table_nested_inside_a_layout_table_survives() {
        let html = r#"<table><tr><td>
            <p>Your receipt</p>
            <table>
              <tr><th>Item</th><th>Qty</th></tr>
              <tr><td>Widget</td><td>2</td></tr>
            </table>
        </td></tr></table>"#;

        let found = tables(html);
        assert_eq!(found.len(), 1, "the outer wrapper unwrapped, the inner one did not");
        assert_eq!(found[0].headers.len(), 2);
        assert!(text_of(&html_to_blocks(html)).contains("Your receipt"));
    }

    #[test]
    fn a_nested_table_does_not_tear_apart_the_row_containing_it() {
        // The failure this guards: a naive `<tr>` scan sees the inner table's
        // rows as the outer table's and produces nonsense.
        let html = r#"<table>
            <tr><td>A</td><td><table><tr><td>inner</td></tr></table></td></tr>
            <tr><td>B</td><td>C</td></tr>
        </table>"#;
        let blocks = html_to_blocks(html);
        let text = text_of(&blocks);
        for expected in ["A", "inner", "B", "C"] {
            assert!(text.contains(expected), "{expected} missing from {text:?}");
        }
    }

    #[test]
    fn an_unclosed_table_still_shows_its_contents() {
        // Mail is full of malformed markup; the unacceptable outcome is a
        // message that renders as nothing.
        let text = text_of(&html_to_blocks("<table><tr><td>orphan"));
        assert!(text.contains("orphan"), "{text:?}");
    }

    #[test]
    fn deeply_nested_tables_terminate() {
        // A pathological message must not recurse until the stack gives out.
        let html = format!(
            "{}<p>deep</p>{}",
            "<table><tr><td>".repeat(60),
            "</td></tr></table>".repeat(60)
        );
        let blocks = html_to_blocks(&html);
        assert!(blocks.len() < 100, "bounded output, got {}", blocks.len());
    }

    #[test]
    fn table_text_reaches_previews_and_search() {
        let html = r#"<table><tr><th>Item</th></tr><tr><td>Widget</td></tr></table>"#;
        let table = Block::Table(tables(html).remove(0));
        assert!(table.text().contains("Item"), "{:?}", table.text());
        assert!(table.text().contains("Widget"));

        // And through the snippet, which is what the message list shows.
        assert!(snippet("", html, 80).contains("Widget"));
    }

    #[test]
    fn cells_outside_a_row_are_kept_rather_than_dropped() {
        let text = text_of(&html_to_blocks("<table><td>stray</td></table>"));
        assert!(text.contains("stray"), "{text:?}");
    }
}
