use super::{Config, HawkbitConfig, identifier};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const MAXIMUM_BYTES: usize = 64 * 1024;
const SYNTAX_ERROR: &str = "invalid Core configuration INI syntax";

#[derive(Default)]
pub(super) struct Document {
    lines: Vec<Line>,
}

struct Line {
    raw: String,
    kind: Kind,
}

enum Kind {
    Comment,
    Section(String),
    Field {
        key: String,
        value: String,
        value_start: usize,
        value_end: usize,
    },
}

impl Document {
    fn parse(text: &str) -> Result<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut document = Self::default();
        let mut physical_lines = text.split_inclusive('\n');
        while let Some(physical_line) = physical_lines.next() {
            let (line, ending) = line_ending(physical_line);
            let mut raw = line.to_owned();
            let trimmed = line.trim();
            let kind = match trimmed.as_bytes().first() {
                None | Some(b'#' | b';') => Kind::Comment,
                Some(b'[') => {
                    let (section, _) = trimmed[1..].rsplit_once(']').context(SYNTAX_ERROR)?;
                    ensure!(!section.is_empty(), SYNTAX_ERROR);
                    Kind::Section(section.into())
                }
                _ => {
                    let (key, separator) = read_key(trimmed)?;
                    let separator = line.len() - line.trim_start().len() + separator;
                    ensure!(!key.is_empty(), SYNTAX_ERROR);
                    let part = &line[separator + 1..];
                    let offset = part.len() - part.trim_start().len();
                    let value_start = separator + 1 + offset;
                    let (value, value_end) =
                        read_value(&mut raw, value_start, ending, &mut physical_lines)?;
                    Kind::Field {
                        key: key.into(),
                        value,
                        value_start,
                        value_end,
                    }
                }
            };
            document.lines.push(Line { raw, kind });
        }
        Ok(document)
    }

    pub(super) fn settings(&self) -> BTreeMap<&str, BTreeMap<&str, &str>> {
        let mut section = "DEFAULT";
        let mut settings = BTreeMap::new();
        for line in &self.lines {
            match &line.kind {
                Kind::Section(name) => {
                    section = name;
                    settings.entry(section).or_insert_with(BTreeMap::new);
                }
                Kind::Field { key, value, .. } => {
                    settings
                        .entry(section)
                        .or_insert_with(BTreeMap::new)
                        .insert(key.as_str(), value.as_str());
                }
                Kind::Comment => {}
            }
        }
        settings
    }

    fn set(&mut self, section: &str, key: &str, value: &str) {
        let mut current = "";
        let mut last_end = None;
        let mut found = false;
        for (index, line) in self.lines.iter_mut().enumerate() {
            match &mut line.kind {
                Kind::Section(name) => {
                    if current == section {
                        last_end = Some(index);
                    }
                    current = name;
                }
                Kind::Field {
                    key: existing,
                    value: old,
                    value_start,
                    value_end,
                } if current == section && existing == key => {
                    // Rewrite every occurrence so previous credentials cannot
                    // survive in a repeated section or as a shadowed key.
                    let suffix = &line.raw[*value_end..];
                    let encoded = encode_value(value, suffix);
                    let comment_needs_separator = suffix.starts_with(['#', ';']);
                    let move_comment = encoded.starts_with("\"\"\"") && suffix.contains("\"\"\"");
                    line.raw.replace_range(*value_start..*value_end, &encoded);
                    *value_end = *value_start + encoded.len();
                    if move_comment {
                        let suffix = line.raw.split_off(*value_end);
                        line.raw.push('\n');
                        if !suffix.trim_start().starts_with(['#', ';']) {
                            line.raw.push_str("# ");
                        }
                        line.raw.push_str(suffix.trim_start());
                    } else if comment_needs_separator {
                        line.raw.insert(*value_end, ' ');
                    }
                    value.clone_into(old);
                    found = true;
                }
                _ => {}
            }
        }
        if found {
            return;
        }
        let encoded = encode_value(value, "");
        let position = if current == section {
            self.lines.len()
        } else if let Some(end) = last_end {
            end
        } else {
            self.lines.push(Line {
                raw: format!("[{section}]"),
                kind: Kind::Section(section.into()),
            });
            self.lines.len()
        };
        self.lines.insert(
            position,
            Line {
                raw: format!("{key}={encoded}"),
                kind: Kind::Field {
                    key: key.into(),
                    value: value.into(),
                    value_start: key.len() + 1,
                    value_end: key.len() + 1 + encoded.len(),
                },
            },
        );
    }

    fn render(&self) -> String {
        let mut result = String::new();
        for line in &self.lines {
            result.push_str(&line.raw);
            result.push('\n');
        }
        result
    }
}

fn line_ending(line: &str) -> (&str, &str) {
    let raw = line
        .strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line);
    (raw, &line[raw.len()..])
}

fn read_key(line: &str) -> Result<(&str, usize)> {
    let quote = if line.len() > 6 && line.starts_with("\"\"\"") {
        Some("\"\"\"")
    } else {
        ["\"", "`"]
            .into_iter()
            .find(|quote| line.starts_with(quote))
    };
    if let Some(quote) = quote {
        let (key, suffix) = line[quote.len()..]
            .split_once(quote)
            .context(SYNTAX_ERROR)?;
        let offset = line.len() - suffix.len();
        let delimiter = suffix.find(['=', ':']).context(SYNTAX_ERROR)?;
        return Ok((key.trim(), offset + delimiter));
    }
    let delimiter = line.find(['=', ':']).context(SYNTAX_ERROR)?;
    Ok((line[..delimiter].trim(), delimiter))
}

fn append_line<'a>(
    raw: &mut String,
    ending: &mut &'a str,
    lines: &mut impl Iterator<Item = &'a str>,
) -> Option<usize> {
    let (next, terminator) = line_ending(lines.next()?);
    raw.push_str(ending);
    let position = raw.len();
    raw.push_str(next);
    *ending = terminator;
    Some(position)
}

fn read_value<'a>(
    raw: &mut String,
    start: usize,
    mut ending: &'a str,
    lines: &mut impl Iterator<Item = &'a str>,
) -> Result<(String, usize)> {
    let text = &raw[start..];
    let quote = if text.len() + ending.len() > 3 && text.starts_with("\"\"\"") {
        Some("\"\"\"")
    } else {
        text.starts_with('`').then_some("`")
    };
    if let Some(quote) = quote {
        let content = start + quote.len();
        if let Some(relative) = raw[content..].rfind(quote) {
            let limit = content + relative;
            return Ok((raw[content..limit].into(), limit + quote.len()));
        }
        while let Some(position) = append_line(raw, &mut ending, lines) {
            if let Some(relative) = raw[position..].rfind(quote) {
                let limit = position + relative;
                if !raw[limit + quote.len()..].trim().ends_with('\\') {
                    return Ok((raw[content..limit].into(), limit + quote.len()));
                }
            }
        }
        bail!(SYNTAX_ERROR);
    }
    let trimmed = text.trim_end();
    if let Some(prefix) = trimmed.strip_suffix('\\') {
        let mut value = prefix.to_owned();
        let mut value_end = start + trimmed.len();
        while let Some(position) = append_line(raw, &mut ending, lines) {
            value_end = raw.len();
            let next = raw[position..].trim();
            if let Some(continued) = next.strip_suffix('\\') {
                value.push_str(continued);
            } else {
                value.push_str(next);
                break;
            }
        }
        return Ok((value, value_end));
    }
    let comment = trimmed
        .find(" #")
        .or_else(|| trimmed.find(" ;"))
        .unwrap_or(trimmed.len());
    let value = trimmed[..comment].trim_end();
    let unquoted = ['"', '\'']
        .into_iter()
        .find_map(|quote| {
            let inner = value.strip_prefix(quote)?.strip_suffix(quote)?;
            (!inner.contains(quote)).then_some(inner)
        })
        .unwrap_or(value);
    Ok((unquoted.into(), start + value.len()))
}

fn encode_value(value: &str, suffix: &str) -> String {
    // Registration fields cannot contain line breaks. Quote tokens which
    // would otherwise be interpreted as delimiters or as a continuation.
    if value.starts_with(['"', '\'', '`']) || value.ends_with('\\') {
        let quote = if suffix.contains('`') { "\"\"\"" } else { "`" };
        return format!("{quote}{value}{quote}");
    }
    value.into()
}

pub(super) fn read(path: &Path) -> Result<Document> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Document::default()),
        Err(error) => return Err(error).context("read Core configuration"),
    };
    ensure!(
        file.metadata()
            .context("inspect Core configuration")?
            .is_file(),
        "Core configuration must be an ordinary file"
    );
    let mut bytes = Vec::new();
    file.take((MAXIMUM_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("read Core configuration")?;
    ensure!(
        bytes.len() <= MAXIMUM_BYTES,
        "Core configuration exceeds 64 KiB"
    );
    ensure!(!bytes.contains(&0), SYNTAX_ERROR);
    Document::parse(std::str::from_utf8(&bytes).map_err(|_| anyhow::anyhow!(SYNTAX_ERROR))?)
}

pub fn ensure_controller_id(
    path: impl AsRef<Path>,
    generate: impl FnOnce() -> Result<String>,
) -> Result<String> {
    let path = path.as_ref();
    edit(path, |document| {
        let current = Config::from_ini(path, document).hawkbit.controller_id;
        if !current.is_empty() {
            return Ok((current, false));
        }
        let id = generate()?;
        ensure!(
            !id.is_empty() && identifier(&id, 128),
            "invalid generated controller identity"
        );
        document.set("hawkbit", "controller_id", &id);
        Ok((id, true))
    })
}

pub fn save_hawkbit_registration(path: impl AsRef<Path>, profile: &HawkbitConfig) -> Result<()> {
    ensure!(
        matches!(profile.token_type.as_str(), "" | "gateway"),
        "registration requires gateway token authentication"
    );
    let fields = [
        ("server_url", profile.server_url.as_str()),
        ("tenant", profile.tenant.as_str()),
        ("controller_id", profile.controller_id.as_str()),
        ("token", profile.token.as_str()),
        ("token_type", "gateway"),
        ("enabled", "true"),
    ];
    let mut validated = Config::default();
    for (key, value) in fields {
        ensure!(
            !value.is_empty()
                && !value.contains(['\0', '\r', '\n'])
                && validated.assign("hawkbit", key, value).is_ok(),
            "invalid hawkBit registration {key}"
        );
    }
    edit(path.as_ref(), |document| {
        for (key, value) in fields {
            let value = if key == "server_url" {
                &validated.hawkbit.server_url
            } else {
                value
            };
            document.set("hawkbit", key, value);
        }
        Ok(((), true))
    })
}

fn edit<T>(path: &Path, change: impl FnOnce(&mut Document) -> Result<(T, bool)>) -> Result<T> {
    ensure!(
        !path.as_os_str().is_empty() && !path.as_os_str().as_encoded_bytes().contains(&0),
        "Core configuration path is required"
    );
    let path = std::path::absolute(path).context("resolve Core configuration path")?;
    let parent = path
        .parent()
        .context("resolve Core configuration directory")?;
    DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(parent)
        .context("create Core configuration directory")?;
    let directory = File::open(parent).context("open Core configuration directory")?;
    lock_directory(&directory).context("lock Core configuration directory")?;
    let mut document = read(&path)?;
    let (value, changed) = change(&mut document)?;
    if changed {
        let updated = document.render();
        ensure!(
            updated.len() <= MAXIMUM_BYTES,
            "updated Core configuration exceeds 64 KiB"
        );
        // Parse before replacing the durable file, including the generated
        // credential encoding.
        Document::parse(&updated)?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".device.ini-")
            .tempfile_in(parent)
            .context("create temporary Core configuration")?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(updated.as_bytes())?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        directory.sync_all()?;
    }
    Ok(value)
}

fn lock_directory(directory: &File) -> io::Result<()> {
    loop {
        // SAFETY: directory remains alive until after the write and fsync.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}
