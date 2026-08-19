use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const OPUS_HEAD: &[u8; 8] = b"OpusHead";
const OPUS_TAGS: &[u8; 8] = b"OpusTags";
pub const PICTURE_TAG: &str = "METADATA_BLOCK_PICTURE";

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Comment {
    pub name: String,
    pub value: String,
}

impl Comment {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let name = name.into();
        validate_field_name(&name)?;
        Ok(Self {
            name,
            value: value.into(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tags {
    pub vendor: String,
    pub comments: Vec<Comment>,
}

impl Tags {
    pub fn read(path: impl AsRef<Path>) -> Result<Self> {
        let input = fs::read(path)?;
        let parsed = ParsedOgg::parse(&input)?;
        Self::parse_packet(&parsed.tags_packet)
    }

    pub fn parse_packet(packet: &[u8]) -> Result<Self> {
        if packet.get(..8) != Some(OPUS_TAGS) {
            return Err(invalid("the second Ogg packet is not OpusTags"));
        }

        let mut cursor = 8;
        let vendor = read_le_string(packet, &mut cursor, "vendor")?;
        let count = read_le_u32(packet, &mut cursor, "comment count")? as usize;
        if count > packet.len().saturating_sub(cursor) / 6 {
            return Err(invalid("OpusTags comment count exceeds packet size"));
        }

        let mut comments = Vec::new();
        for index in 0..count {
            let raw = read_le_string(packet, &mut cursor, "comment")?;
            let Some((name, value)) = raw.split_once('=') else {
                return Err(invalid(format!("comment {index} has no '=' separator")));
            };
            validate_field_name(name)?;
            comments
                .try_reserve(1)
                .map_err(|_| invalid("OpusTags comment list is too large"))?;
            comments.push(Comment {
                name: name.to_owned(),
                value: value.to_owned(),
            });
        }

        Ok(Self { vendor, comments })
    }

    pub fn to_packet(&self) -> Result<Vec<u8>> {
        let mut packet = Vec::new();
        packet.extend_from_slice(OPUS_TAGS);
        push_le_string(&mut packet, &self.vendor)?;
        let count = u32::try_from(self.comments.len())
            .map_err(|_| invalid("too many OpusTags comments"))?;
        packet.extend_from_slice(&count.to_le_bytes());
        for comment in &self.comments {
            validate_field_name(&comment.name)?;
            let length = comment
                .name
                .len()
                .checked_add(1)
                .and_then(|length| length.checked_add(comment.value.len()))
                .ok_or_else(|| invalid("comment is too large"))?;
            let length = u32::try_from(length).map_err(|_| invalid("comment is too large"))?;
            packet.extend_from_slice(&length.to_le_bytes());
            packet.extend_from_slice(comment.name.as_bytes());
            packet.push(b'=');
            packet.extend_from_slice(comment.value.as_bytes());
        }
        Ok(packet)
    }

    pub fn write(&self, input: impl AsRef<Path>, output: Option<&Path>) -> Result<()> {
        let source = fs::canonicalize(input.as_ref())?;
        let original = fs::read(&source)?;
        let parsed = ParsedOgg::parse(&original)?;
        write_tags(&source, output, &parsed, self)
    }
}

pub fn edit_file(
    input: impl AsRef<Path>,
    output: Option<&Path>,
    change: impl FnOnce(&mut Tags) -> Result<()>,
) -> Result<()> {
    let source = fs::canonicalize(input.as_ref())?;
    let original = fs::read(&source)?;
    let parsed = ParsedOgg::parse(&original)?;
    let mut tags = Tags::parse_packet(&parsed.tags_packet)?;
    change(&mut tags)?;
    write_tags(&source, output, &parsed, &tags)
}

fn write_tags(source: &Path, output: Option<&Path>, parsed: &ParsedOgg, tags: &Tags) -> Result<()> {
    #[cfg(unix)]
    {
        let replaces_source_entry = match output {
            None => true,
            Some(destination) => same_directory_entry(source, destination)?,
        };
        if replaces_source_entry && fs::metadata(source)?.nlink() > 1 {
            return Err(invalid(
                "refusing an in-place edit of a multiply linked file; use a separate output path to avoid silently breaking hard-link associations",
            ));
        }
    }
    let updated = parsed.rewrite(&tags.to_packet()?)?;
    let destination = output
        .map(Path::to_path_buf)
        .unwrap_or_else(|| source.to_path_buf());
    let permissions = match fs::metadata(&destination) {
        Ok(metadata) => metadata.permissions(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::metadata(source)?.permissions()
        }
        Err(error) => return Err(error.into()),
    };
    atomic_write(&destination, &updated, permissions)
}

#[cfg(unix)]
fn same_directory_entry(left: &Path, right: &Path) -> Result<bool> {
    fn resolve_without_final_symlink(path: &Path) -> Result<PathBuf> {
        let name = path
            .file_name()
            .ok_or_else(|| invalid("output path has no file name"))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Ok(fs::canonicalize(parent)?.join(name))
    }

    Ok(resolve_without_final_symlink(left)? == resolve_without_final_symlink(right)?)
}

pub fn write_audio_packets(input: impl AsRef<Path>, mut output: impl Write) -> Result<usize> {
    let input = fs::read(input)?;
    let parsed = ParsedOgg::parse(&input)?;
    parsed.write_audio_packets(&mut output)
}

pub fn write_audio_packets_file(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<usize> {
    let input = fs::read(input)?;
    let parsed = ParsedOgg::parse(&input)?;
    let output = output.as_ref();
    let permissions = existing_permissions(output)?;
    atomic_write_with(output, permissions, |file| parsed.write_audio_packets(file))
}

pub fn write_output_file(output: impl AsRef<Path>, data: &[u8]) -> Result<()> {
    let output = output.as_ref();
    let permissions = existing_permissions(output)?;
    atomic_write_with(output, permissions, |file| {
        file.write_all(data)?;
        Ok(())
    })
}

pub fn validate_field_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(invalid("tag field name cannot be empty"));
    }
    if !name
        .bytes()
        .all(|byte| (0x20..=0x7d).contains(&byte) && byte != b'=')
    {
        return Err(invalid(
            "tag field names must use printable ASCII characters except '='",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Picture {
    pub picture_type: u32,
    pub mime_type: String,
    pub description: String,
    pub width: u32,
    pub height: u32,
    pub color_depth: u32,
    pub indexed_colors: u32,
    pub data: Vec<u8>,
}

impl Picture {
    pub fn from_image(data: Vec<u8>, description: String) -> Result<Self> {
        let (mime_type, width, height, color_depth, indexed_colors) = image_info(&data)
            .ok_or_else(|| {
                invalid("unrecognized or structurally invalid PNG, JPEG, GIF, or WebP cover")
            })?;
        Ok(Self {
            picture_type: 3,
            mime_type: mime_type.to_owned(),
            description,
            width,
            height,
            color_depth,
            indexed_colors,
            data,
        })
    }

    pub fn from_comment(value: &str) -> Result<Self> {
        Self::from_block(&base64_decode(value)?)
    }

    pub fn to_comment(&self) -> Result<String> {
        Ok(base64_encode(&self.to_block()?))
    }

    pub fn from_block(block: &[u8]) -> Result<Self> {
        let mut cursor = 0;
        let picture_type = read_be_u32(block, &mut cursor, "picture type")?;
        let mime_type = read_be_string(block, &mut cursor, "picture MIME type")?;
        let description = read_be_string(block, &mut cursor, "picture description")?;
        let width = read_be_u32(block, &mut cursor, "picture width")?;
        let height = read_be_u32(block, &mut cursor, "picture height")?;
        let color_depth = read_be_u32(block, &mut cursor, "picture color depth")?;
        let indexed_colors = read_be_u32(block, &mut cursor, "picture color count")?;
        let length = read_be_u32(block, &mut cursor, "picture data length")? as usize;
        let end = cursor
            .checked_add(length)
            .ok_or_else(|| invalid("picture data length overflows"))?;
        let data = block
            .get(cursor..end)
            .ok_or_else(|| invalid("picture data is truncated"))?
            .to_vec();
        if end != block.len() {
            return Err(invalid("picture block contains trailing data"));
        }
        Ok(Self {
            picture_type,
            mime_type,
            description,
            width,
            height,
            color_depth,
            indexed_colors,
            data,
        })
    }

    pub fn to_block(&self) -> Result<Vec<u8>> {
        let mut block = Vec::new();
        block.extend_from_slice(&self.picture_type.to_be_bytes());
        push_be_string(&mut block, &self.mime_type)?;
        push_be_string(&mut block, &self.description)?;
        block.extend_from_slice(&self.width.to_be_bytes());
        block.extend_from_slice(&self.height.to_be_bytes());
        block.extend_from_slice(&self.color_depth.to_be_bytes());
        block.extend_from_slice(&self.indexed_colors.to_be_bytes());
        let length = u32::try_from(self.data.len()).map_err(|_| invalid("picture is too large"))?;
        block.extend_from_slice(&length.to_be_bytes());
        block.extend_from_slice(&self.data);
        Ok(block)
    }
}

#[derive(Clone, Debug)]
struct Page {
    header_type: u8,
    granule_position: u64,
    serial: u32,
    sequence: u32,
    segments: Vec<u8>,
    data: Vec<u8>,
}

impl Page {
    fn parse_all(input: &[u8]) -> Result<Vec<Self>> {
        let mut pages = Vec::new();
        let mut cursor = 0usize;
        while cursor < input.len() {
            if input.get(cursor..cursor + 4) != Some(b"OggS") {
                return Err(invalid(format!(
                    "missing Ogg page capture pattern at byte {cursor}"
                )));
            }
            let fixed = input
                .get(cursor..cursor + 27)
                .ok_or_else(|| invalid("truncated Ogg page header"))?;
            if fixed[4] != 0 {
                return Err(invalid(format!("unsupported Ogg version {}", fixed[4])));
            }
            let segment_count = fixed[26] as usize;
            let header_end = cursor + 27 + segment_count;
            let segments = input
                .get(cursor + 27..header_end)
                .ok_or_else(|| invalid("truncated Ogg segment table"))?
                .to_vec();
            let payload_length: usize = segments.iter().map(|&length| length as usize).sum();
            let page_end = header_end
                .checked_add(payload_length)
                .ok_or_else(|| invalid("Ogg page size overflows"))?;
            let raw = input
                .get(cursor..page_end)
                .ok_or_else(|| invalid("truncated Ogg page payload"))?;
            let expected_crc = u32::from_le_bytes(fixed[22..26].try_into().unwrap());
            let mut crc_input = raw.to_vec();
            crc_input[22..26].fill(0);
            if ogg_crc(&crc_input) != expected_crc {
                return Err(invalid(format!("CRC mismatch in Ogg page {}", pages.len())));
            }
            pages.push(Self {
                header_type: fixed[5],
                granule_position: u64::from_le_bytes(fixed[6..14].try_into().unwrap()),
                serial: u32::from_le_bytes(fixed[14..18].try_into().unwrap()),
                sequence: u32::from_le_bytes(fixed[18..22].try_into().unwrap()),
                segments,
                data: input[header_end..page_end].to_vec(),
            });
            cursor = page_end;
        }
        if pages.is_empty() {
            return Err(invalid("file contains no Ogg pages"));
        }
        Ok(pages)
    }

    fn encode(&self) -> Result<Vec<u8>> {
        if self.segments.len() > 255 {
            return Err(invalid("an Ogg page cannot contain more than 255 segments"));
        }
        let expected: usize = self.segments.iter().map(|&length| length as usize).sum();
        if expected != self.data.len() {
            return Err(invalid("Ogg segment table does not match page payload"));
        }
        let mut output = Vec::with_capacity(27 + self.segments.len() + self.data.len());
        output.extend_from_slice(b"OggS");
        output.push(0);
        output.push(self.header_type);
        output.extend_from_slice(&self.granule_position.to_le_bytes());
        output.extend_from_slice(&self.serial.to_le_bytes());
        output.extend_from_slice(&self.sequence.to_le_bytes());
        output.extend_from_slice(&0u32.to_le_bytes());
        output.push(self.segments.len() as u8);
        output.extend_from_slice(&self.segments);
        output.extend_from_slice(&self.data);
        let checksum = ogg_crc(&output);
        output[22..26].copy_from_slice(&checksum.to_le_bytes());
        Ok(output)
    }
}

#[derive(Debug)]
struct ParsedOgg {
    pages: Vec<Page>,
    head_packet: Vec<u8>,
    tags_packet: Vec<u8>,
    tags_end_page: usize,
    tags_end_segment: usize,
}

impl ParsedOgg {
    fn parse(input: &[u8]) -> Result<Self> {
        let pages = Page::parse_all(input)?;
        let serial = pages[0].serial;
        if pages.iter().any(|page| page.serial != serial) {
            return Err(invalid(
                "multiplexed or chained Ogg streams are not supported; expected one Opus logical stream",
            ));
        }
        for (index, page) in pages.iter().enumerate() {
            let expected = pages[0].sequence.wrapping_add(index as u32);
            if page.sequence != expected {
                return Err(invalid(format!(
                    "non-contiguous Ogg page sequence at page {index}"
                )));
            }
        }
        validate_page_framing(&pages)?;

        let mut packets = vec![Vec::new(), Vec::new()];
        let mut packet_index = 0usize;
        let mut tags_end = None;
        for (page_index, page) in pages.iter().enumerate() {
            let mut data_cursor = 0usize;
            for (segment_index, &length) in page.segments.iter().enumerate() {
                let length = length as usize;
                let bytes = &page.data[data_cursor..data_cursor + length];
                if packet_index < 2 {
                    packets[packet_index].extend_from_slice(bytes);
                }
                data_cursor += length;
                if length < 255 {
                    if packet_index == 0
                        && (page_index != 0 || segment_index + 1 != page.segments.len())
                    {
                        return Err(invalid(
                            "OpusHead must be the only packet on the first Ogg page",
                        ));
                    }
                    if packet_index == 1 {
                        tags_end = Some((page_index, segment_index));
                        break;
                    }
                    packet_index += 1;
                }
            }
            if tags_end.is_some() {
                break;
            }
        }
        let (tags_end_page, tags_end_segment) =
            tags_end.ok_or_else(|| invalid("file does not contain a complete OpusTags packet"))?;
        if packets[0].get(..8) != Some(OPUS_HEAD) {
            return Err(invalid("the first Ogg packet is not OpusHead"));
        }
        validate_opus_head(&packets[0])?;
        if packets[1].get(..8) != Some(OPUS_TAGS) {
            return Err(invalid("the second Ogg packet is not OpusTags"));
        }
        Ok(Self {
            pages,
            head_packet: packets.remove(0),
            tags_packet: packets.remove(0),
            tags_end_page,
            tags_end_segment,
        })
    }

    fn rewrite(&self, new_tags: &[u8]) -> Result<Vec<u8>> {
        if new_tags.get(..8) != Some(OPUS_TAGS) {
            return Err(invalid("replacement packet is not OpusTags"));
        }
        let serial = self.pages[0].serial;
        let original_end = &self.pages[self.tags_end_page];
        let suffix_segments = original_end.segments[self.tags_end_segment + 1..].to_vec();
        let suffix_offset: usize = original_end.segments[..=self.tags_end_segment]
            .iter()
            .map(|&length| length as usize)
            .sum();
        let suffix_data = original_end.data[suffix_offset..].to_vec();
        let no_later_pages = self.tags_end_page + 1 == self.pages.len();
        let tags_was_eos = original_end.header_type & 0x04 != 0;

        let mut rebuilt = Vec::new();
        let mut sequence = self.pages[0].sequence;
        paginate_packet(
            &self.head_packet,
            serial,
            &mut sequence,
            true,
            false,
            &mut rebuilt,
        )?;
        paginate_packet(
            new_tags,
            serial,
            &mut sequence,
            false,
            tags_was_eos && suffix_segments.is_empty() && no_later_pages,
            &mut rebuilt,
        )?;

        if !suffix_segments.is_empty() {
            rebuilt.push(Page {
                header_type: original_end.header_type & 0x04,
                granule_position: original_end.granule_position,
                serial,
                sequence,
                segments: suffix_segments,
                data: suffix_data,
            });
            sequence = sequence.wrapping_add(1);
        }
        for original in &self.pages[self.tags_end_page + 1..] {
            let mut page = original.clone();
            page.sequence = sequence;
            rebuilt.push(page);
            sequence = sequence.wrapping_add(1);
        }

        let mut output = Vec::new();
        for page in rebuilt {
            output.extend_from_slice(&page.encode()?);
        }
        Ok(output)
    }

    fn write_audio_packets(&self, mut output: impl Write) -> Result<usize> {
        let mut packet = Vec::new();
        let mut packet_index = 0usize;
        let mut audio_count = 0usize;
        let mut packet_continues = false;

        for page in &self.pages {
            let mut data_cursor = 0usize;
            for &length in &page.segments {
                let length = length as usize;
                packet.extend_from_slice(&page.data[data_cursor..data_cursor + length]);
                data_cursor += length;
                packet_continues = length == 255;
                if !packet_continues {
                    if packet_index >= 2 {
                        let length = u64::try_from(packet.len())
                            .map_err(|_| invalid("Opus packet is too large"))?;
                        output.write_all(&length.to_le_bytes())?;
                        output.write_all(&packet)?;
                        audio_count += 1;
                    }
                    packet.clear();
                    packet_index += 1;
                }
            }
        }
        if packet_continues {
            return Err(invalid("file ends in an incomplete Ogg packet"));
        }
        Ok(audio_count)
    }
}

fn validate_page_framing(pages: &[Page]) -> Result<()> {
    let mut packet_continues = false;
    for (index, page) in pages.iter().enumerate() {
        if page.header_type & !0x07 != 0 {
            return Err(invalid(format!("unknown Ogg header flags on page {index}")));
        }
        let is_first = index == 0;
        let is_last = index + 1 == pages.len();
        if (page.header_type & 0x02 != 0) != is_first {
            return Err(invalid(format!("invalid Ogg BOS flag on page {index}")));
        }
        if (page.header_type & 0x04 != 0) != is_last {
            return Err(invalid(format!("invalid Ogg EOS flag on page {index}")));
        }
        let continued = page.header_type & 0x01 != 0;
        if page.segments.is_empty() {
            if continued {
                return Err(invalid(format!(
                    "empty Ogg page {index} is marked as continued"
                )));
            }
        } else if continued != packet_continues {
            return Err(invalid(format!(
                "Ogg continuation flag does not match packet boundary on page {index}"
            )));
        }
        if let Some(&last_segment) = page.segments.last() {
            packet_continues = last_segment == 255;
        }
    }
    if packet_continues {
        return Err(invalid("file ends in an incomplete Ogg packet"));
    }
    Ok(())
}

fn validate_opus_head(packet: &[u8]) -> Result<()> {
    if packet.len() < 19 {
        return Err(invalid("OpusHead packet is shorter than 19 bytes"));
    }
    if packet[8] >= 16 {
        return Err(invalid(format!(
            "unsupported OpusHead version {}",
            packet[8]
        )));
    }
    let channels = packet[9] as usize;
    if channels == 0 {
        return Err(invalid("OpusHead channel count is zero"));
    }
    let mapping_family = packet[18];
    if mapping_family == 0 {
        if channels > 2 {
            return Err(invalid(
                "OpusHead mapping family 0 supports only one or two channels",
            ));
        }
        return Ok(());
    }
    if mapping_family == 1 && channels > 8 {
        return Err(invalid(
            "OpusHead mapping family 1 supports at most eight channels",
        ));
    }
    let required = 21usize
        .checked_add(channels)
        .ok_or_else(|| invalid("OpusHead channel mapping size overflows"))?;
    if packet.len() < required {
        return Err(invalid("truncated OpusHead channel mapping table"));
    }
    let streams = packet[19] as usize;
    let coupled = packet[20] as usize;
    let coded_channels = streams
        .checked_add(coupled)
        .ok_or_else(|| invalid("OpusHead coded channel count overflows"))?;
    if streams == 0 || coupled > streams || coded_channels > 255 {
        return Err(invalid("invalid OpusHead stream counts"));
    }
    if packet[21..required]
        .iter()
        .any(|&index| index != 255 && index as usize >= coded_channels)
    {
        return Err(invalid("invalid OpusHead channel mapping index"));
    }
    Ok(())
}

fn paginate_packet(
    packet: &[u8],
    serial: u32,
    sequence: &mut u32,
    bos: bool,
    eos: bool,
    output: &mut Vec<Page>,
) -> Result<()> {
    let mut lacing = vec![255u8; packet.len() / 255];
    lacing.push((packet.len() % 255) as u8);
    let mut lace_cursor = 0usize;
    let mut data_cursor = 0usize;
    let mut first = true;
    while lace_cursor < lacing.len() {
        let lace_end = (lace_cursor + 255).min(lacing.len());
        let segments = lacing[lace_cursor..lace_end].to_vec();
        let data_length: usize = segments.iter().map(|&length| length as usize).sum();
        let data_end = data_cursor + data_length;
        let last = lace_end == lacing.len();
        let mut header_type = 0;
        if !first {
            header_type |= 0x01;
        }
        if bos && first {
            header_type |= 0x02;
        }
        if eos && last {
            header_type |= 0x04;
        }
        output.push(Page {
            header_type,
            granule_position: if last { 0 } else { u64::MAX },
            serial,
            sequence: *sequence,
            segments,
            data: packet[data_cursor..data_end].to_vec(),
        });
        *sequence = sequence.wrapping_add(1);
        lace_cursor = lace_end;
        data_cursor = data_end;
        first = false;
    }
    Ok(())
}

fn atomic_write(path: &Path, data: &[u8], permissions: fs::Permissions) -> Result<()> {
    atomic_write_with(path, Some(permissions), |file| {
        file.write_all(data)?;
        Ok(())
    })
}

fn existing_permissions(path: &Path) -> Result<Option<fs::Permissions>> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn atomic_write_with<T>(
    path: &Path,
    permissions: Option<fs::Permissions>,
    write: impl FnOnce(&mut File) -> Result<T>,
) -> Result<T> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("opustagger-output");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut temp_path = PathBuf::new();
    let mut temp = None;
    for attempt in 0..100u32 {
        temp_path = parent.join(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            nonce + attempt as u128
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temp_path) {
            Ok(file) => {
                temp = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let mut temp = temp.ok_or_else(|| invalid("could not create a temporary output file"))?;
    let result = (|| -> Result<T> {
        let value = write(&mut temp)?;
        if let Some(permissions) = permissions {
            temp.set_permissions(permissions)?;
        }
        temp.sync_all()?;
        drop(temp);
        fs::rename(&temp_path, path)?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(value)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn read_le_u32(input: &[u8], cursor: &mut usize, what: &str) -> Result<u32> {
    let bytes = input
        .get(*cursor..*cursor + 4)
        .ok_or_else(|| invalid(format!("truncated {what}")))?;
    *cursor += 4;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn read_be_u32(input: &[u8], cursor: &mut usize, what: &str) -> Result<u32> {
    let bytes = input
        .get(*cursor..*cursor + 4)
        .ok_or_else(|| invalid(format!("truncated {what}")))?;
    *cursor += 4;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
}

fn read_le_string(input: &[u8], cursor: &mut usize, what: &str) -> Result<String> {
    let length = read_le_u32(input, cursor, what)? as usize;
    read_utf8(input, cursor, length, what)
}

fn read_be_string(input: &[u8], cursor: &mut usize, what: &str) -> Result<String> {
    let length = read_be_u32(input, cursor, what)? as usize;
    read_utf8(input, cursor, length, what)
}

fn read_utf8(input: &[u8], cursor: &mut usize, length: usize, what: &str) -> Result<String> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| invalid(format!("{what} length overflows")))?;
    let bytes = input
        .get(*cursor..end)
        .ok_or_else(|| invalid(format!("truncated {what}")))?;
    *cursor = end;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| invalid(format!("{what} is not valid UTF-8")))
}

fn push_le_string(output: &mut Vec<u8>, value: &str) -> Result<()> {
    let length = u32::try_from(value.len()).map_err(|_| invalid("string is too large"))?;
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn push_be_string(output: &mut Vec<u8>, value: &str) -> Result<()> {
    let length = u32::try_from(value.len()).map_err(|_| invalid("string is too large"))?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn ogg_crc(input: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in input {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(BASE64[(a >> 2) as usize] as char);
        output.push(BASE64[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            BASE64[(((b & 15) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            BASE64[(c & 63) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn base64_decode(input: &str) -> Result<Vec<u8>> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(invalid("picture tag contains invalid base64 length"));
    }
    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    for (chunk_index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = chunk_index + 1 == bytes.len() / 4;
        let a = decode_base64_byte(chunk[0])?;
        let b = decode_base64_byte(chunk[1])?;
        let c_padding = chunk[2] == b'=';
        let d_padding = chunk[3] == b'=';
        if c_padding && !d_padding {
            return Err(invalid("picture tag contains invalid base64 padding"));
        }
        if (c_padding || d_padding) && !last {
            return Err(invalid(
                "picture tag contains base64 padding before its end",
            ));
        }
        let c = if c_padding {
            0
        } else {
            decode_base64_byte(chunk[2])?
        };
        let d = if d_padding {
            0
        } else {
            decode_base64_byte(chunk[3])?
        };
        output.push((a << 2) | (b >> 4));
        if !c_padding {
            output.push((b << 4) | (c >> 2));
        }
        if !d_padding {
            output.push((c << 6) | d);
        }
    }
    Ok(output)
}

fn decode_base64_byte(byte: u8) -> Result<u8> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(invalid("picture tag contains invalid base64")),
    }
}

fn image_info(data: &[u8]) -> Option<(&'static str, u32, u32, u32, u32)> {
    png_info(data)
        .or_else(|| jpeg_info(data))
        .or_else(|| gif_info(data))
        .or_else(|| webp_info(data))
}

fn png_info(data: &[u8]) -> Option<(&'static str, u32, u32, u32, u32)> {
    if data.get(..8) != Some(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut cursor = 8usize;
    let mut dimensions = None;
    let mut color_type = None;
    let mut indexed_max = None;
    let mut indexed_colors = None;
    let mut has_image_data = false;
    while cursor + 12 <= data.len() {
        let length = u32::from_be_bytes(data[cursor..cursor + 4].try_into().ok()?) as usize;
        let kind = data.get(cursor + 4..cursor + 8)?;
        let payload_end = cursor.checked_add(8)?.checked_add(length)?;
        let chunk_end = payload_end.checked_add(4)?;
        let chunk = data.get(cursor + 4..payload_end)?;
        let expected_crc = u32::from_be_bytes(data.get(payload_end..chunk_end)?.try_into().ok()?);
        if png_crc(chunk) != expected_crc {
            return None;
        }

        match kind {
            b"IHDR" if cursor == 8 && length == 13 => {
                let width = u32::from_be_bytes(data[16..20].try_into().ok()?);
                let height = u32::from_be_bytes(data[20..24].try_into().ok()?);
                let bits = data[24];
                let png_color_type = data[25];
                let channels = match (png_color_type, bits) {
                    (0, 1 | 2 | 4 | 8 | 16) => 1,
                    (2, 8 | 16) => 3,
                    (3, 1 | 2 | 4 | 8) => 1,
                    (4, 8 | 16) => 2,
                    (6, 8 | 16) => 4,
                    _ => return None,
                };
                if width == 0 || height == 0 || data[26] != 0 || data[27] != 0 || data[28] > 1 {
                    return None;
                }
                dimensions = Some((width, height, bits as u32 * channels));
                indexed_max = (png_color_type == 3).then_some(1u32 << bits);
                color_type = Some(png_color_type);
            }
            b"IHDR" => return None,
            b"PLTE"
                if dimensions.is_some()
                    && !has_image_data
                    && matches!(color_type, Some(2 | 3 | 6))
                    && indexed_colors.is_none()
                    && length > 0
                    && length.is_multiple_of(3) =>
            {
                let colors = u32::try_from(length / 3).ok()?;
                if colors > 256 || indexed_max.is_some_and(|maximum| colors > maximum) {
                    return None;
                }
                indexed_colors = Some(colors);
            }
            b"PLTE" => return None,
            b"IDAT"
                if dimensions.is_some() && (indexed_max.is_none() || indexed_colors.is_some()) =>
            {
                has_image_data = true;
            }
            b"IEND" if length == 0 => {
                let (width, height, depth) = dimensions?;
                let colors = if indexed_max.is_some() {
                    indexed_colors?
                } else {
                    0
                };
                return (has_image_data && chunk_end == data.len()).then_some((
                    "image/png",
                    width,
                    height,
                    depth,
                    colors,
                ));
            }
            _ if dimensions.is_none() => return None,
            _ => {}
        }
        cursor = chunk_end;
    }
    None
}

fn png_crc(input: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in input {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn jpeg_info(data: &[u8]) -> Option<(&'static str, u32, u32, u32, u32)> {
    if data.get(..2) != Some(b"\xff\xd8")
        || data.get(data.len().checked_sub(2)?..) != Some(b"\xff\xd9")
    {
        return None;
    }
    let (width, height, depth) = jpeg_dimensions(data)?;
    (width > 0 && height > 0 && depth > 0).then_some(("image/jpeg", width, height, depth, 0))
}

fn gif_info(data: &[u8]) -> Option<(&'static str, u32, u32, u32, u32)> {
    if data.get(..6) != Some(b"GIF87a") && data.get(..6) != Some(b"GIF89a") {
        return None;
    }
    let width = u16::from_le_bytes(data.get(6..8)?.try_into().ok()?) as u32;
    let height = u16::from_le_bytes(data.get(8..10)?.try_into().ok()?) as u32;
    let packed = *data.get(10)?;
    let mut table_colors = if packed & 0x80 != 0 {
        1u32 << ((packed & 0x07) + 1)
    } else {
        0
    };
    let table_bytes = table_colors as usize * 3;
    let mut cursor = 13usize.checked_add(table_bytes)?;
    if width == 0 || height == 0 || cursor > data.len() {
        return None;
    }
    let mut has_image = false;
    loop {
        match *data.get(cursor)? {
            0x21 => {
                cursor += 2;
                skip_gif_sub_blocks(data, &mut cursor, false)?;
            }
            0x2c => {
                let descriptor = data.get(cursor..cursor + 10)?;
                let image_width = u16::from_le_bytes(descriptor[5..7].try_into().ok()?);
                let image_height = u16::from_le_bytes(descriptor[7..9].try_into().ok()?);
                if image_width == 0 || image_height == 0 {
                    return None;
                }
                cursor += 10;
                let local_packed = descriptor[9];
                if local_packed & 0x80 != 0 {
                    let local_colors = 1u32 << ((local_packed & 0x07) + 1);
                    table_colors = table_colors.max(local_colors);
                    cursor = cursor.checked_add(local_colors as usize * 3)?;
                    data.get(..cursor)?;
                }
                let code_size = *data.get(cursor)?;
                if !(2..=8).contains(&code_size) {
                    return None;
                }
                cursor += 1;
                skip_gif_sub_blocks(data, &mut cursor, true)?;
                has_image = true;
            }
            0x3b if has_image && cursor + 1 == data.len() => break,
            _ => return None,
        }
    }
    let depth = ((packed >> 4) & 0x07) as u32 + 1;
    Some(("image/gif", width, height, depth, table_colors))
}

fn skip_gif_sub_blocks(data: &[u8], cursor: &mut usize, require_data: bool) -> Option<()> {
    let mut has_data = false;
    loop {
        let length = *data.get(*cursor)? as usize;
        *cursor += 1;
        if length == 0 {
            return (!require_data || has_data).then_some(());
        }
        *cursor = cursor.checked_add(length)?;
        data.get(..*cursor)?;
        has_data = true;
    }
}

fn webp_info(data: &[u8]) -> Option<(&'static str, u32, u32, u32, u32)> {
    if data.get(..4) != Some(b"RIFF") || data.get(8..12) != Some(b"WEBP") {
        return None;
    }
    let riff_length = u32::from_le_bytes(data.get(4..8)?.try_into().ok()?) as usize;
    if riff_length.checked_add(8)? != data.len() {
        return None;
    }

    let mut cursor = 12usize;
    let mut dimensions = None;
    let mut has_image_payload = false;
    while cursor + 8 <= data.len() {
        let kind = data.get(cursor..cursor + 4)?;
        let length =
            u32::from_le_bytes(data.get(cursor + 4..cursor + 8)?.try_into().ok()?) as usize;
        let payload_start = cursor + 8;
        let payload_end = payload_start.checked_add(length)?;
        let payload = data.get(payload_start..payload_end)?;
        match kind {
            b"VP8X" if payload.len() >= 10 => {
                let width = 1 + read_le_u24(&payload[4..7]);
                let height = 1 + read_le_u24(&payload[7..10]);
                let depth = if payload[0] & 0x10 != 0 { 32 } else { 24 };
                dimensions = Some((width, height, depth));
            }
            b"VP8 " if payload.len() > 10 && payload[3..6] == [0x9d, 0x01, 0x2a] => {
                let width = u16::from_le_bytes(payload[6..8].try_into().ok()?) as u32 & 0x3fff;
                let height = u16::from_le_bytes(payload[8..10].try_into().ok()?) as u32 & 0x3fff;
                let depth = dimensions.map_or(24, |(_, _, depth)| depth);
                dimensions = Some((width, height, depth));
                has_image_payload = true;
            }
            b"VP8L" if payload.len() > 5 && payload[0] == 0x2f => {
                let packed = u32::from_le_bytes(payload[1..5].try_into().ok()?);
                let width = (packed & 0x3fff) + 1;
                let height = ((packed >> 14) & 0x3fff) + 1;
                let depth = if packed & (1 << 28) != 0 { 32 } else { 24 };
                dimensions = Some((width, height, depth));
                has_image_payload = true;
            }
            b"ANMF" if webp_frame_has_image(payload) => has_image_payload = true,
            _ => {}
        }
        cursor = payload_end.checked_add(length & 1)?;
    }
    let (width, height, depth) = dimensions?;
    (has_image_payload && cursor == data.len() && width > 0 && height > 0).then_some((
        "image/webp",
        width,
        height,
        depth,
        0,
    ))
}

fn webp_frame_has_image(frame: &[u8]) -> bool {
    let mut cursor = 16usize;
    while cursor + 8 <= frame.len() {
        let Some(kind) = frame.get(cursor..cursor + 4) else {
            return false;
        };
        let Some(length) = frame.get(cursor + 4..cursor + 8) else {
            return false;
        };
        let Ok(length) = <[u8; 4]>::try_from(length) else {
            return false;
        };
        let length = u32::from_le_bytes(length) as usize;
        let payload_start = cursor + 8;
        let Some(payload_end) = payload_start.checked_add(length) else {
            return false;
        };
        let Some(payload) = frame.get(payload_start..payload_end) else {
            return false;
        };
        if (kind == b"VP8 " && payload.len() > 10 && payload[3..6] == [0x9d, 0x01, 0x2a])
            || (kind == b"VP8L" && payload.len() > 5 && payload[0] == 0x2f)
        {
            return true;
        }
        let Some(next) = payload_end.checked_add(length & 1) else {
            return false;
        };
        cursor = next;
    }
    false
}

fn read_le_u24(bytes: &[u8]) -> u32 {
    bytes[0] as u32 | (bytes[1] as u32) << 8 | (bytes[2] as u32) << 16
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32, u32)> {
    let mut cursor = 2usize;
    let mut dimensions = None;
    while cursor + 4 <= data.len() {
        if data[cursor] != 0xff {
            cursor += 1;
            continue;
        }
        while data.get(cursor) == Some(&0xff) {
            cursor += 1;
        }
        let marker = *data.get(cursor)?;
        cursor += 1;
        if marker == 0x01 || marker == 0xd8 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        if marker == 0xd9 {
            return None;
        }
        let length = u16::from_be_bytes(data.get(cursor..cursor + 2)?.try_into().ok()?) as usize;
        if length < 2 || cursor + length > data.len() {
            return None;
        }
        if marker == 0xda {
            if length < 6 {
                return None;
            }
            let components = *data.get(cursor + 2)? as usize;
            let expected_length = 6usize.checked_add(components.checked_mul(2)?)?;
            if components == 0 || length != expected_length {
                return None;
            }
            let scan_start = cursor.checked_add(length)?;
            let eoi_start = data.len().checked_sub(2)?;
            return (scan_start < eoi_start).then_some(dimensions?);
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if length < 8 {
                return None;
            }
            let sample_bits = *data.get(cursor + 2)? as u32;
            let height =
                u16::from_be_bytes(data.get(cursor + 3..cursor + 5)?.try_into().ok()?) as u32;
            let width =
                u16::from_be_bytes(data.get(cursor + 5..cursor + 7)?.try_into().ok()?) as u32;
            let components = *data.get(cursor + 7)? as u32;
            let expected_length = 8usize.checked_add(components as usize * 3)?;
            if components == 0 || length != expected_length {
                return None;
            }
            dimensions = Some((width, height, sample_bits * components));
        }
        cursor += length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
        output.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        let crc_start = output.len();
        output.extend_from_slice(kind);
        output.extend_from_slice(payload);
        let crc = png_crc(&output[crc_start..]);
        output.extend_from_slice(&crc.to_be_bytes());
    }

    fn indexed_png(palette_after_data: bool, duplicate_palette: bool) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&1u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 3, 0, 0, 0]);
        push_png_chunk(&mut png, b"IHDR", &ihdr);
        let palette = [0, 0, 0, 255, 255, 255];
        if !palette_after_data {
            push_png_chunk(&mut png, b"PLTE", &palette);
        }
        let compressed_scanline = [0x78, 0x01, 0x01, 0x02, 0, 0xfd, 0xff, 0, 0, 0, 0, 0, 1];
        push_png_chunk(&mut png, b"IDAT", &compressed_scanline);
        if palette_after_data || duplicate_palette {
            push_png_chunk(&mut png, b"PLTE", &palette);
        }
        push_png_chunk(&mut png, b"IEND", &[]);
        png
    }

    fn opus_head() -> Vec<u8> {
        let mut packet = OPUS_HEAD.to_vec();
        packet.extend_from_slice(&[1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        packet
    }

    fn fixture(tags: &Tags, audio: &[u8]) -> Vec<u8> {
        let mut pages = Vec::new();
        let mut sequence = 0;
        paginate_packet(&opus_head(), 7, &mut sequence, true, false, &mut pages).unwrap();
        paginate_packet(
            &tags.to_packet().unwrap(),
            7,
            &mut sequence,
            false,
            false,
            &mut pages,
        )
        .unwrap();
        pages.push(Page {
            header_type: 0x04,
            granule_position: 960,
            serial: 7,
            sequence,
            segments: vec![audio.len() as u8],
            data: audio.to_vec(),
        });
        pages
            .into_iter()
            .flat_map(|page| page.encode().unwrap())
            .collect()
    }

    fn encode_pages(pages: &[Page]) -> Vec<u8> {
        pages
            .iter()
            .flat_map(|page| page.encode().unwrap())
            .collect()
    }

    #[cfg(unix)]
    fn unique_test_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("opustagger-{label}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn tags_round_trip_repeated_fields_and_equals() {
        let tags = Tags {
            vendor: "encoder 1.0".into(),
            comments: vec![
                Comment::new("ARTIST", "One").unwrap(),
                Comment::new("artist", "Two").unwrap(),
                Comment::new("URL", "https://example.test/?a=b").unwrap(),
            ],
        };
        assert_eq!(
            Tags::parse_packet(&tags.to_packet().unwrap()).unwrap(),
            tags
        );

        let mut impossible_count = OPUS_TAGS.to_vec();
        impossible_count.extend_from_slice(&0u32.to_le_bytes());
        impossible_count.extend_from_slice(&2u32.to_le_bytes());
        impossible_count.extend_from_slice(&[0; 8]);
        assert!(Tags::parse_packet(&impossible_count).is_err());
    }

    #[test]
    fn picture_round_trip() {
        let picture = Picture {
            picture_type: 3,
            mime_type: "image/png".into(),
            description: "Front".into(),
            width: 10,
            height: 20,
            color_depth: 32,
            indexed_colors: 0,
            data: vec![0, 1, 2, 253, 254, 255],
        };
        assert_eq!(
            Picture::from_comment(&picture.to_comment().unwrap()).unwrap(),
            picture
        );
        assert!(Picture::from_image(vec![1, 2, 3], String::new()).is_err());
        assert!(Picture::from_image(vec![0xff, 0xd8, 0xff, 0xd9], String::new()).is_err());
        let malformed_sof = vec![0xff, 0xd8, 0xff, 0xc0, 0, 2, 8, 0, 1, 0, 1, 1, 0xff, 0xd9];
        assert!(Picture::from_image(malformed_sof, String::new()).is_err());
        let sof_without_scan = vec![
            0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0, 0xff, 0xd9,
        ];
        assert!(Picture::from_image(sof_without_scan, String::new()).is_err());
        let jpeg_with_tem = vec![
            0xff, 0xd8, 0xff, 0x01, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0, 0xff, 0xda, 0,
            8, 1, 1, 0, 0, 0x3f, 0, 0, 0xff, 0xd9,
        ];
        let picture = Picture::from_image(jpeg_with_tem, "JPEG with TEM".into()).unwrap();
        assert_eq!((picture.width, picture.height), (1, 1));
        let malformed_sos = vec![
            0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 0, 1, 0, 1, 1, 1, 0x11, 0, 0xff, 0xda, 0, 2, 0, 0xff,
            0xd9,
        ];
        assert!(Picture::from_image(malformed_sos, String::new()).is_err());
        assert!(Picture::from_image(b"RIFF\x04\0\0\0WEBP".to_vec(), String::new()).is_err());
        let mut header_only_webp = b"RIFF".to_vec();
        header_only_webp.extend_from_slice(&22u32.to_le_bytes());
        header_only_webp.extend_from_slice(b"WEBPVP8X");
        header_only_webp.extend_from_slice(&10u32.to_le_bytes());
        header_only_webp.extend_from_slice(&[0; 10]);
        assert!(Picture::from_image(header_only_webp, String::new()).is_err());

        let png = base64_decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
        )
        .unwrap();
        let picture = Picture::from_image(png, "valid PNG".into()).unwrap();
        assert_eq!((picture.width, picture.height), (1, 1));
        assert_eq!(picture.mime_type, "image/png");

        let picture = Picture::from_image(indexed_png(false, false), "indexed PNG".into()).unwrap();
        assert_eq!(picture.indexed_colors, 2);
        assert!(Picture::from_image(indexed_png(true, false), String::new()).is_err());
        assert!(Picture::from_image(indexed_png(false, true), String::new()).is_err());
        let mut duplicate_header = indexed_png(false, false);
        let ihdr_chunk = duplicate_header[8..33].to_vec();
        duplicate_header.splice(33..33, ihdr_chunk);
        assert!(Picture::from_image(duplicate_header, String::new()).is_err());

        let mut webp_body = b"WEBPVP8X".to_vec();
        webp_body.extend_from_slice(&10u32.to_le_bytes());
        webp_body.extend_from_slice(&[0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        webp_body.extend_from_slice(b"VP8 ");
        webp_body.extend_from_slice(&11u32.to_le_bytes());
        webp_body.extend_from_slice(&[0, 0, 0, 0x9d, 0x01, 0x2a, 1, 0, 1, 0, 0]);
        webp_body.push(0);
        let mut alpha_webp = b"RIFF".to_vec();
        alpha_webp.extend_from_slice(&(webp_body.len() as u32).to_le_bytes());
        alpha_webp.extend_from_slice(&webp_body);
        let picture = Picture::from_image(alpha_webp, "alpha WebP".into()).unwrap();
        assert_eq!(picture.color_depth, 32);

        let gif = base64_decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==").unwrap();
        let picture = Picture::from_image(gif, "valid GIF".into()).unwrap();
        assert_eq!((picture.width, picture.height), (1, 1));
        assert_eq!(picture.mime_type, "image/gif");
        let invalid_gif = b"GIF89a\x01\0\x01\0\0\0\0\x2c\x3b".to_vec();
        assert!(Picture::from_image(invalid_gif, String::new()).is_err());
    }

    #[test]
    fn rewrite_preserves_audio_packet_bytes() {
        let original_tags = Tags {
            vendor: "test".into(),
            comments: vec![Comment::new("TITLE", "Before").unwrap()],
        };
        let audio = [0xf8, 0xff, 1, 2, 3, 4];
        let input = fixture(&original_tags, &audio);
        let parsed = ParsedOgg::parse(&input).unwrap();
        let updated_tags = Tags {
            vendor: "new vendor".into(),
            comments: (0..1000)
                .map(|index| Comment::new("COMMENT", format!("entry-{index}")))
                .collect::<Result<_>>()
                .unwrap(),
        };
        let output = parsed.rewrite(&updated_tags.to_packet().unwrap()).unwrap();
        let reparsed = ParsedOgg::parse(&output).unwrap();
        let mut original_packets = Vec::new();
        let mut rewritten_packets = Vec::new();
        parsed.write_audio_packets(&mut original_packets).unwrap();
        reparsed
            .write_audio_packets(&mut rewritten_packets)
            .unwrap();
        assert_eq!(original_packets, rewritten_packets);
        assert_eq!(
            Tags::parse_packet(&reparsed.tags_packet).unwrap(),
            updated_tags
        );
        let last = reparsed.pages.last().unwrap();
        assert_eq!(last.data, audio);
        assert_eq!(last.granule_position, 960);
        assert_eq!(last.header_type & 0x04, 0x04);
    }

    #[test]
    fn rewrite_preserves_audio_sharing_the_last_tag_page() {
        let tags = Tags {
            vendor: "test".into(),
            comments: vec![Comment::new("TITLE", "Before").unwrap()],
        };
        let tags_packet = tags.to_packet().unwrap();
        let audio = vec![0xf8, 0xff, 1, 2, 3];
        let mut pages = Vec::new();
        let mut sequence = 0;
        paginate_packet(&opus_head(), 9, &mut sequence, true, false, &mut pages).unwrap();
        pages.push(Page {
            header_type: 0x04,
            granule_position: 960,
            serial: 9,
            sequence,
            segments: vec![tags_packet.len() as u8, audio.len() as u8],
            data: [tags_packet, audio.clone()].concat(),
        });
        let input: Vec<u8> = pages
            .into_iter()
            .flat_map(|page| page.encode().unwrap())
            .collect();

        let parsed = ParsedOgg::parse(&input).unwrap();
        let mut updated = tags;
        updated.comments[0].value = "After".repeat(200);
        let output = parsed.rewrite(&updated.to_packet().unwrap()).unwrap();
        let reparsed = ParsedOgg::parse(&output).unwrap();
        let last = reparsed.pages.last().unwrap();
        assert_eq!(last.data, audio);
        assert_eq!(last.granule_position, 960);
        assert_eq!(last.header_type & 0x04, 0x04);
    }

    #[test]
    fn invalid_ogg_framing_is_rejected() {
        let tags = Tags {
            vendor: "test".into(),
            comments: vec![],
        };
        let original = fixture(&tags, &[1, 2]);
        let pages = Page::parse_all(&original).unwrap();

        let mut missing_bos = pages.clone();
        missing_bos[0].header_type &= !0x02;
        assert!(
            ParsedOgg::parse(&encode_pages(&missing_bos))
                .unwrap_err()
                .to_string()
                .contains("BOS")
        );

        let mut spurious_continuation = pages.clone();
        spurious_continuation[1].header_type |= 0x01;
        assert!(
            ParsedOgg::parse(&encode_pages(&spurious_continuation))
                .unwrap_err()
                .to_string()
                .contains("continuation")
        );

        let mut missing_eos = pages.clone();
        missing_eos.last_mut().unwrap().header_type &= !0x04;
        assert!(
            ParsedOgg::parse(&encode_pages(&missing_eos))
                .unwrap_err()
                .to_string()
                .contains("EOS")
        );

        let mut incomplete_packet = pages;
        let last = incomplete_packet.last_mut().unwrap();
        last.segments = vec![255];
        last.data = vec![0; 255];
        assert!(
            ParsedOgg::parse(&encode_pages(&incomplete_packet))
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
    }

    #[test]
    fn truncated_opus_head_is_rejected() {
        let tags = Tags {
            vendor: "test".into(),
            comments: vec![],
        };
        let original = fixture(&tags, &[1, 2]);
        let mut pages = Page::parse_all(&original).unwrap();
        pages[0].segments = vec![OPUS_HEAD.len() as u8];
        pages[0].data = OPUS_HEAD.to_vec();
        assert!(
            ParsedOgg::parse(&encode_pages(&pages))
                .unwrap_err()
                .to_string()
                .contains("shorter than 19 bytes")
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_output_is_private_while_being_written() {
        use std::os::unix::fs::PermissionsExt;

        let path = unique_test_path("private-temp");
        fs::write(&path, b"before").unwrap();
        let final_permissions = fs::Permissions::from_mode(0o644);
        atomic_write_with(&path, Some(final_permissions), |file| {
            assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
            file.write_all(b"after")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(&path).unwrap(), b"after");
        fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn in_place_edit_refuses_multiply_linked_input() {
        let path = unique_test_path("linked-input");
        let alias = path.with_extension("alias");
        let tags = Tags {
            vendor: "test".into(),
            comments: vec![],
        };
        let original = fixture(&tags, &[1, 2]);
        fs::write(&path, &original).unwrap();
        fs::hard_link(&path, &alias).unwrap();

        let error = tags.write(&path, None).unwrap_err();
        assert!(error.to_string().contains("multiply linked"));
        let error = tags.write(&path, Some(&path)).unwrap_err();
        assert!(error.to_string().contains("multiply linked"));
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read(&alias).unwrap(), original);

        let mut updated = tags;
        updated.vendor = "updated".into();
        updated.write(&path, Some(&alias)).unwrap();
        assert_eq!(Tags::read(&path).unwrap().vendor, "test");
        assert_eq!(Tags::read(&alias).unwrap().vendor, "updated");

        fs::remove_file(alias).unwrap();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn crc_corruption_is_rejected() {
        let tags = Tags {
            vendor: "test".into(),
            comments: vec![],
        };
        let mut input = fixture(&tags, &[1, 2]);
        *input.last_mut().unwrap() ^= 1;
        assert!(
            ParsedOgg::parse(&input)
                .unwrap_err()
                .to_string()
                .contains("CRC mismatch")
        );
    }
}
