//! Display-only SMBIOS fields, per DMTF DSP0134 3.9, types 0, 2, 16 and 17.
//! https://www.dmtf.org/sites/default/files/standards/documents/DSP0134_3.9.0.pdf
//! Serial numbers, UUIDs and asset tags are never extracted.
use super::capacity;

const MAX_TABLE: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Details {
    pub memory: String,
    pub modules: String,
    pub board: String,
    pub bios: String,
}

#[cfg(any(windows, target_os = "linux"))]
pub(super) fn detect() -> Details {
    let details = table().and_then(|data| parse(&data)).unwrap_or_default();
    #[cfg(target_os = "linux")]
    let mut details = details;
    #[cfg(target_os = "linux")]
    {
        let field = |key| {
            std::fs::read_to_string(format!("/sys/class/dmi/id/{key}"))
                .ok()
                .and_then(|s| clean(&s))
        };
        if details.board.is_empty() {
            details.board = join([field("board_vendor"), field("board_name")]);
        }
        if details.bios.is_empty() {
            details.bios = join([
                field("bios_vendor"),
                field("bios_version"),
                field("bios_date"),
            ]);
        }
    }
    details
}

#[cfg(windows)]
fn table() -> Option<Vec<u8>> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemFirmwareTable(provider: u32, id: u32, buffer: *mut c_void, bytes: u32) -> u32;
    }
    // Microsoft RSMB returns an 8-byte RawSMBIOSData header followed by the table.
    // https://learn.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-getsystemfirmwaretable
    let provider = u32::from_be_bytes(*b"RSMB");
    let size = unsafe { GetSystemFirmwareTable(provider, 0, std::ptr::null_mut(), 0) } as usize;
    if !(8..=MAX_TABLE + 8).contains(&size) {
        return None;
    }
    let mut data = vec![0u8; size];
    let written =
        unsafe { GetSystemFirmwareTable(provider, 0, data.as_mut_ptr().cast(), size as u32) }
            as usize;
    if written < 8 || written > size {
        return None;
    }
    let length = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    if length > MAX_TABLE || length > written - 8 {
        return None;
    }
    data.drain(..8);
    data.truncate(length);
    Some(data)
}

#[cfg(target_os = "linux")]
fn table() -> Option<Vec<u8>> {
    use std::io::Read;
    let mut data = Vec::new();
    std::fs::File::open("/sys/firmware/dmi/tables/DMI")
        .ok()?
        .take(MAX_TABLE as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    (data.len() <= MAX_TABLE).then_some(data)
}

fn clean(text: &str) -> Option<String> {
    let value = text.chars().filter(|c| !c.is_control()).collect::<String>();
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty()
        || [
            "unknown",
            "none",
            "not specified",
            "not available",
            "default string",
            "to be filled by o.e.m.",
            "to be filled by oem",
        ]
        .iter()
        .any(|s| value.eq_ignore_ascii_case(s))
    {
        None
    } else {
        Some(value)
    }
}

fn join<const N: usize>(fields: [Option<String>; N]) -> String {
    fields.into_iter().flatten().collect::<Vec<_>>().join(" ")
}

fn word(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn dword(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn device_size(data: &[u8]) -> Option<u64> {
    let size = word(data, 0x0c)?;
    match size {
        0 | 0x8000 | 0xffff => None,
        0x7fff => {
            let extended = dword(data, 0x1c)?;
            // Bit 31 is reserved; zero is not a populated device capacity.
            (extended != 0 && extended & 0x8000_0000 == 0).then_some(u64::from(extended) << 20)
        }
        _ => Some(u64::from(size & 0x7fff) << if size & 0x8000 != 0 { 10 } else { 20 }),
    }
}

fn configured_speed(data: &[u8]) -> Option<u32> {
    let speed = match word(data, 0x20)? {
        0 => return None,
        0xffff => dword(data, 0x58)?,
        value => u32::from(value),
    };
    (speed != 0 && speed & 0x8000_0000 == 0).then_some(speed)
}

fn memory_type(value: u8) -> &'static str {
    match value {
        0x0f => "SDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x19 => "FBD2",
        0x1a => "DDR4",
        0x1b => "LPDDR",
        0x1c => "LPDDR2",
        0x1d => "LPDDR3",
        0x1e => "LPDDR4",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        0x25 => "MRDIMM",
        _ => "",
    }
}

fn add_group(groups: &mut Vec<(String, usize)>, text: String) {
    if text.is_empty() {
        return;
    }
    if let Some((_, count)) = groups.iter_mut().find(|(key, _)| *key == text) {
        *count += 1;
    } else {
        groups.push((text, 1));
    }
}

fn groups_text(groups: Vec<(String, usize)>) -> String {
    groups
        .into_iter()
        .map(|(text, count)| format!("{count} × {text}"))
        .collect::<Vec<_>>()
        .join("; ")
}

fn parse(mut data: &[u8]) -> Option<Details> {
    if data.len() > MAX_TABLE {
        return None;
    }
    let mut details = Details::default();
    let mut main_board = false;
    let mut arrays = std::collections::HashMap::new();
    let mut devices = Vec::new();
    while !data.is_empty() {
        let kind = *data.first()?;
        let length = usize::from(*data.get(1)?);
        if length < 4 || (kind == 17 && length < 0x15) {
            return None;
        }
        let fields = data.get(..length)?;
        let tail = data.get(length..)?;
        let strings_end = tail.windows(2).position(|w| w == [0, 0])?;
        let strings = &tail[..strings_end];
        let text = |offset| -> Option<String> {
            let index = usize::from(*fields.get(offset)?);
            if index == 0 {
                return None;
            }
            clean(&String::from_utf8_lossy(
                strings.split(|&b| b == 0).nth(index - 1)?,
            ))
        };
        match kind {
            0 if details.bios.is_empty() => details.bios = join([text(4), text(5), text(8)]),
            2 if !main_board => {
                let hosting = fields.get(9).is_some_and(|flags| flags & 1 != 0);
                if details.board.is_empty() || hosting {
                    details.board = join([text(4), text(5)]);
                    main_board = hosting && !details.board.is_empty();
                }
            }
            16 => {
                arrays.insert(word(fields, 2)?, *fields.get(5)?);
                if arrays.len() > 4096 {
                    return None;
                }
            }
            17 if word(fields, 0x0c).is_some_and(|size| size != 0)
                && fields.get(0x12) != Some(&0x1f) =>
            {
                devices.push((fields, join([text(0x17), text(0x1a)])));
                if devices.len() > 4096 {
                    return None;
                }
            }
            _ => {}
        }
        if kind == 127 {
            break;
        }
        data = &tail[strings_end + 2..];
    }
    let mut sizes = Vec::new();
    let mut identities = Vec::new();
    let mut total = Some(0u64);
    for (fields, identity) in devices {
        // Arrays can occur after their devices. A known non-system array is not RAM.
        if arrays
            .get(&word(fields, 4)?)
            .is_some_and(|&usage| usage != 3)
        {
            continue;
        }
        let mut bytes = device_size(fields);
        // A mixed persistent device's full Size is not installed volatile RAM.
        if word(fields, 0x13).is_some_and(|bits| bits & (1 << 12) != 0) {
            bytes = fields
                .get(0x3c..0x44)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .filter(|&n| n != 0 && n != u64::MAX);
        }
        total = total.and_then(|sum| sum.checked_add(bytes?));
        let mut summary = bytes.map(capacity).unwrap_or_else(|| "size unknown".into());
        let kind = memory_type(*fields.get(0x12).unwrap_or(&0));
        if !kind.is_empty() {
            summary.push_str(&format!(" {kind}"));
        }
        if let Some(speed) = configured_speed(fields) {
            summary.push_str(&format!(" @ {speed} MT/s"));
        }
        add_group(&mut sizes, summary);
        add_group(&mut identities, identity);
    }
    if !sizes.is_empty()
        && let Some(total) = total.filter(|&n| n != 0)
    {
        details.memory = format!("{} installed · {}", capacity(total), groups_text(sizes));
    }
    details.modules = groups_text(identities);
    Some(details)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u8, length: usize, strings: &[&str]) -> Vec<u8> {
        let mut bytes = vec![0; length];
        bytes[0] = kind;
        bytes[1] = length as u8;
        for text in strings {
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0);
        }
        if strings.is_empty() {
            bytes.push(0);
        }
        bytes.push(0);
        bytes
    }

    #[test]
    fn populated_devices_are_grouped_without_empty_slots_or_serials() {
        let mut module = record(17, 0x5c, &["Maker", "SECRET-SERIAL", "PART-32"]);
        module[0x0c..0x0e].copy_from_slice(&0x7fffu16.to_le_bytes());
        module[0x1c..0x20].copy_from_slice(&32768u32.to_le_bytes());
        module[0x12] = 0x22;
        module[0x15..0x17].copy_from_slice(&6400u16.to_le_bytes());
        module[0x20..0x22].copy_from_slice(&6000u16.to_le_bytes());
        module[0x17] = 1;
        module[0x18] = 2;
        module[0x1a] = 3;
        let mut table = module.clone();
        table.extend(module);
        table.extend(record(17, 0x15, &[])); // unpopulated slot
        let details = parse(&table).unwrap();
        assert_eq!(
            details.memory,
            "64 GiB installed · 2 × 32 GiB DDR5 @ 6000 MT/s"
        );
        assert_eq!(details.modules, "2 × Maker PART-32");
        assert!(!details.modules.contains("SECRET"));
    }

    #[test]
    fn sizes_and_configured_speeds_respect_sentinels_and_record_bounds() {
        let mut fields = vec![0u8; 0x5c];
        fields[0x0c..0x0e].copy_from_slice(&0x8000u16.to_le_bytes());
        assert_eq!(device_size(&fields), None);
        fields[0x0c..0x0e].copy_from_slice(&0x8100u16.to_le_bytes());
        assert_eq!(device_size(&fields), Some(256 << 10));
        fields[0x0c..0x0e].copy_from_slice(&0x7fffu16.to_le_bytes());
        assert_eq!(device_size(&fields[..0x1c]), None);
        fields[0x1c..0x20].copy_from_slice(&131072u32.to_le_bytes());
        assert_eq!(device_size(&fields), Some(128 << 30));
        fields[0x1c + 3] |= 0x80;
        assert_eq!(device_size(&fields), None);
        fields[0x20..0x22].copy_from_slice(&0xffffu16.to_le_bytes());
        fields[0x58..0x5c].copy_from_slice(&80000u32.to_le_bytes());
        assert_eq!(configured_speed(&fields), Some(80000));
        assert_eq!(configured_speed(&fields[..0x58]), None);
        fields[0x20..0x22].fill(0);
        assert_eq!(configured_speed(&fields), None);
    }

    #[test]
    fn malformed_tables_do_not_produce_partial_inventory() {
        for table in [
            &[17][..],
            &[17, 3, 0, 0, 0, 0],
            &[17, 20, 0, 0, 0, 0],
            &[0, 4, 0, 0, b'x', 0],
        ] {
            assert!(parse(table).is_none());
        }
        let mut valid = record(0, 9, &["Vendor", "Version", "01/02/2026"]);
        valid[4] = 1;
        valid[5] = 2;
        valid[8] = 3;
        assert_eq!(parse(&valid).unwrap().bios, "Vendor Version 01/02/2026");
        valid.push(17);
        assert!(parse(&valid).is_none());
    }

    #[test]
    fn non_system_arrays_do_not_inflate_ram_even_when_described_later() {
        let mut memory = record(17, 0x22, &[]);
        memory[0x0c..0x0e].copy_from_slice(&8192u16.to_le_bytes());
        memory[4..6].copy_from_slice(&1u16.to_le_bytes());
        let mut video = memory.clone();
        video[4..6].copy_from_slice(&2u16.to_le_bytes());
        let mut system_array = record(16, 0x0f, &[]);
        system_array[2..4].copy_from_slice(&1u16.to_le_bytes());
        system_array[5] = 3;
        let mut video_array = system_array.clone();
        video_array[2..4].copy_from_slice(&2u16.to_le_bytes());
        video_array[5] = 4;
        let table = [memory, video, system_array, video_array].concat();
        assert_eq!(parse(&table).unwrap().memory, "8 GiB installed · 1 × 8 GiB");
    }
}
