use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use anyhow::{Result, anyhow, ensure};
use serde::Deserialize;

const MAX_METADATA: usize = 64 * 1024;
const MAX_ATTRIBUTE_VALUE: usize = 1024;

type Attributes = BTreeMap<String, String>;

#[derive(Deserialize)]
struct Firmware {
    version: String,
    #[serde(default)]
    build_id: String,
    #[serde(default)]
    machine: String,
}

pub(super) fn read_system_attributes(version: &Path, os_release: &Path) -> Result<Attributes> {
    let mut attrs = Attributes::new();
    let firmware_known = match read_metadata(version) {
        Ok(data) => {
            let firmware: Firmware = serde_json::from_slice(&data)
                .map_err(|_| anyhow!("invalid system version metadata"))?;
            ensure!(
                !firmware.version.trim().is_empty(),
                "invalid system version metadata"
            );
            attrs.insert("system_name".into(), "NAOS".into());
            for (name, value) in [
                ("system_version", firmware.version),
                ("system_build_id", firmware.build_id),
                ("system_machine", firmware.machine),
            ] {
                let value = value.trim();
                ensure!(
                    valid_attribute_value(value),
                    "invalid system version metadata"
                );
                insert(&mut attrs, name, value);
            }
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => return Err(anyhow!("cannot read system version metadata")),
    };
    let os_info = match parse_os_release(os_release) {
        Ok(info) => info,
        Err(_) if firmware_known => return Ok(attrs),
        Err(_) => return Err(anyhow!("cannot read system version metadata")),
    };
    let name = first_value(&os_info, &["NAME", "PRETTY_NAME", "ID"]);
    let version = first_value(&os_info, &["VERSION_ID", "VERSION"]);
    let id = first_value(&os_info, &["ID"]);
    insert(&mut attrs, "os_name", name);
    insert(&mut attrs, "os_version", version);
    insert(&mut attrs, "os_id", id);
    if !firmware_known {
        ensure!(!version.is_empty(), "system version is unavailable");
        insert(&mut attrs, "system_version", version);
        insert(&mut attrs, "system_name", name);
        insert(&mut attrs, "system_id", id);
    }
    Ok(attrs)
}

fn insert(attrs: &mut Attributes, name: &str, value: &str) {
    if !value.is_empty() {
        attrs.insert(name.into(), value.into());
    }
}

fn read_metadata(path: &Path) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)?
        .take((MAX_METADATA + 1) as u64)
        .read_to_end(&mut data)?;
    if data.len() > MAX_METADATA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "version metadata too large",
        ));
    }
    Ok(data)
}

fn parse_os_release(path: &Path) -> Result<Attributes> {
    let data = String::from_utf8(read_metadata(path)?)?;
    let mut values = Attributes::new();
    for line in data.lines() {
        let Some((key, raw)) = line.trim().split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !matches!(
            key,
            "VERSION_ID" | "VERSION" | "NAME" | "PRETTY_NAME" | "ID"
        ) {
            continue;
        }
        let value = release_value(raw.trim())?;
        let trimmed = value.trim();
        ensure!(valid_attribute_value(trimmed), "invalid os-release value");
        values.insert(key.into(), trimmed.into());
    }
    Ok(values)
}

fn release_value(raw: &str) -> Result<String> {
    let mut raw = raw.char_indices().peekable();
    let mut value = String::new();
    let quote = match raw.peek() {
        Some((_, quote @ ('\'' | '"'))) => Some(*quote),
        _ => None,
    };
    if quote.is_some() {
        raw.next();
    }
    while let Some((index, character)) = raw.next() {
        let ends_value = quote == Some(character)
            || quote.is_none()
                && (matches!(character, ' ' | '\t') || character == '#' && index == 0);
        if ends_value {
            let tail: String = std::iter::once(character)
                .chain(raw.map(|(_, character)| character))
                .collect();
            let tail = if quote.is_some() { &tail[1..] } else { &tail };
            let tail = tail.trim();
            ensure!(
                tail.is_empty() || tail.starts_with('#'),
                "invalid os-release value"
            );
            return Ok(value);
        }
        if character == '\\' && quote != Some('\'') {
            let (_, next) = raw
                .peek()
                .copied()
                .ok_or_else(|| anyhow!("invalid escaped os-release value"))?;
            if quote.is_none() || matches!(next, '\\' | '"' | '$' | '`') {
                value.push(next);
                raw.next();
                continue;
            }
        }
        value.push(character);
    }
    ensure!(quote.is_none(), "unterminated os-release value");
    Ok(value)
}

fn first_value<'a>(values: &'a Attributes, keys: &[&str]) -> &'a str {
    keys.iter()
        .filter_map(|key| values.get(*key))
        .find(|value| !value.is_empty())
        .map(String::as_str)
        .unwrap_or_default()
}

pub(super) fn valid_attribute_value(value: &str) -> bool {
    value.len() <= MAX_ATTRIBUTE_VALUE && value.bytes().all(|byte| byte >= 32 && byte != 127)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn metadata_version_and_os_version_are_independent() {
        let folder = tempfile::tempdir().unwrap();
        let metadata = folder.path().join("version.json");
        let release = folder.path().join("os-release");
        fs::write(
            &metadata,
            r#"{"version":" 1.2.3 ","build_id":"test","machine":"q900","extra":99}"#,
        )
        .unwrap();
        fs::write(&release, "NAME=Alpine\nVERSION_ID=3.24\nID=alpine\n").unwrap();
        let attrs = read_system_attributes(&metadata, &release).unwrap();
        assert_eq!(attrs["system_name"], "NAOS");
        assert_eq!(attrs["system_version"], "1.2.3");
        assert_eq!(attrs["system_build_id"], "test");
        assert_eq!(attrs["system_machine"], "q900");
        assert_eq!(attrs["os_version"], "3.24");
        assert_eq!(attrs["os_name"], "Alpine");
        assert_eq!(attrs["os_id"], "alpine");
        assert!(!attrs.contains_key("system_id"));
        fs::remove_file(release).unwrap();
        assert_eq!(
            read_system_attributes(&metadata, &folder.path().join("os-release"))
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn only_missing_firmware_metadata_allows_os_fallback() {
        let folder = tempfile::tempdir().unwrap();
        let metadata = folder.path().join("version.json");
        let release = folder.path().join("os-release");
        fs::write(
            &release,
            "PRETTY_NAME=\"Alpine Linux\"\nVERSION=\"3.24\"\nID=alpine\n",
        )
        .unwrap();
        let attrs = read_system_attributes(&metadata, &release).unwrap();
        assert_eq!(attrs["system_version"], "3.24");
        assert_eq!(attrs["system_name"], "Alpine Linux");
        assert_eq!(attrs["system_id"], "alpine");
        for contents in [
            "{invalid}",
            r#"{"version":" "}"#,
            r#"{"version":"x\nY"}"#,
            r#"{"version":12}"#,
        ] {
            fs::write(&metadata, contents).unwrap();
            assert!(read_system_attributes(&metadata, &release).is_err());
        }
        fs::write(&metadata, " ".repeat(MAX_METADATA + 1)).unwrap();
        assert!(read_system_attributes(&metadata, &release).is_err());
    }

    #[test]
    fn release_parser_handles_literal_shell_looking_strings() {
        assert_eq!(
            release_value(r#""$(touch /tmp/no) $VAR" # comment"#).unwrap(),
            "$(touch /tmp/no) $VAR"
        );
        assert_eq!(
            release_value(r#"'Alpine \\ Linux' # base"#).unwrap(),
            r#"Alpine \\ Linux"#
        );
        assert_eq!(
            release_value(r#"Alpine\ Linux\#test"#).unwrap(),
            "Alpine Linux#test"
        );
        assert_eq!(release_value("Alpine#test").unwrap(), "Alpine#test");
        assert!(release_value(r#""Alpine" invalid"#).is_err());
        assert!(release_value("'Alpine").is_err());
        assert!(release_value("Alpine Linux").is_err());
    }
}
