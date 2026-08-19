use opustagger::{
    Comment, Error, PICTURE_TAG, Picture, Result, Tags, edit_file, write_audio_packets_file,
    write_output_file,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const HELP: &str = "\
opustagger — edit Ogg Opus metadata without re-encoding audio

USAGE:
  opustagger show FILE
  opustagger set FILE FIELD VALUE [-o FILE]
  opustagger add FILE FIELD VALUE [-o FILE]
  opustagger edit FILE TAG_INDEX FIELD VALUE [-o FILE]
  opustagger remove FILE TAG_INDEX [-o FILE]
  opustagger vendor FILE VALUE [-o FILE]
  opustagger cover-add FILE IMAGE [DESCRIPTION] [-o FILE]
  opustagger cover-list FILE
  opustagger cover-extract FILE COVER_INDEX OUTPUT
  opustagger cover-remove FILE COVER_INDEX [-o FILE]
  opustagger audio-dump FILE OUTPUT

Tag indices are printed by `show`; cover indices are printed by `cover-list`.
`set` replaces every case-insensitive occurrence of FIELD with one value. `add`
preserves repeated fields. Mutating commands replace FILE atomically unless -o
or --output is supplied. Use -- before a literal -o or --output argument.
";

fn main() {
    if let Err(error) = run(env::args().skip(1).collect()) {
        eprintln!("opustagger: {error}");
        std::process::exit(1);
    }
}

fn run(mut args: Vec<String>) -> Result<()> {
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print!("{HELP}");
        return Ok(());
    }
    if matches!(args[0].as_str(), "-V" | "--version") {
        println!("opustagger {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let command = args.remove(0);
    match command.as_str() {
        "show" => {
            exact_args(&args, 1, "show FILE")?;
            show(&args[0])
        }
        "cover-list" => {
            exact_args(&args, 1, "cover-list FILE")?;
            list_covers(&args[0])
        }
        "cover-extract" => {
            exact_args(&args, 3, "cover-extract FILE COVER_INDEX OUTPUT")?;
            extract_cover(&args[0], parse_index(&args[1], "cover")?, &args[2])
        }
        "audio-dump" => {
            exact_args(&args, 2, "audio-dump FILE OUTPUT")?;
            if paths_refer_to_same_file(Path::new(&args[0]), Path::new(&args[1]))? {
                return Err(Error::Invalid(
                    "audio-dump output must not refer to its input file".into(),
                ));
            }
            write_audio_packets_file(&args[0], &args[1])?;
            Ok(())
        }
        "set" => {
            let (args, output) = output_option(args)?;
            exact_args(&args, 3, "set FILE FIELD VALUE [-o FILE]")?;
            mutate(&args[0], output.as_deref(), |tags| {
                let replacement = Comment::new(&args[1], &args[2])?;
                let first = tags
                    .comments
                    .iter()
                    .position(|comment| comment.name.eq_ignore_ascii_case(&args[1]));
                tags.comments
                    .retain(|comment| !comment.name.eq_ignore_ascii_case(&args[1]));
                tags.comments
                    .insert(first.unwrap_or(tags.comments.len()), replacement);
                Ok(())
            })
        }
        "add" => {
            let (args, output) = output_option(args)?;
            exact_args(&args, 3, "add FILE FIELD VALUE [-o FILE]")?;
            mutate(&args[0], output.as_deref(), |tags| {
                tags.comments.push(Comment::new(&args[1], &args[2])?);
                Ok(())
            })
        }
        "edit" => {
            let (args, output) = output_option(args)?;
            exact_args(&args, 4, "edit FILE TAG_INDEX FIELD VALUE [-o FILE]")?;
            let index = parse_index(&args[1], "tag")?;
            mutate(&args[0], output.as_deref(), |tags| {
                let length = tags.comments.len();
                let comment = tags.comments.get_mut(index).ok_or_else(|| {
                    Error::Invalid(format!("tag index {index} is out of range (0..{length})"))
                })?;
                *comment = Comment::new(&args[2], &args[3])?;
                Ok(())
            })
        }
        "remove" => {
            let (args, output) = output_option(args)?;
            exact_args(&args, 2, "remove FILE TAG_INDEX [-o FILE]")?;
            let index = parse_index(&args[1], "tag")?;
            mutate(&args[0], output.as_deref(), |tags| {
                if index >= tags.comments.len() {
                    return Err(Error::Invalid(format!(
                        "tag index {index} is out of range (0..{})",
                        tags.comments.len()
                    )));
                }
                tags.comments.remove(index);
                Ok(())
            })
        }
        "vendor" => {
            let (args, output) = output_option(args)?;
            exact_args(&args, 2, "vendor FILE VALUE [-o FILE]")?;
            mutate(&args[0], output.as_deref(), |tags| {
                tags.vendor.clone_from(&args[1]);
                Ok(())
            })
        }
        "cover-add" => {
            let (args, output) = output_option(args)?;
            if !(2..=3).contains(&args.len()) {
                return Err(usage("cover-add FILE IMAGE [DESCRIPTION] [-o FILE]"));
            }
            let image = fs::read(&args[1])?;
            let description = args.get(2).cloned().unwrap_or_default();
            let picture_value = Picture::from_image(image, description)?.to_comment()?;
            mutate(&args[0], output.as_deref(), |tags| {
                tags.comments
                    .push(Comment::new(PICTURE_TAG, picture_value)?);
                Ok(())
            })
        }
        "cover-remove" => {
            let (args, output) = output_option(args)?;
            exact_args(&args, 2, "cover-remove FILE COVER_INDEX [-o FILE]")?;
            let cover_index = parse_index(&args[1], "cover")?;
            mutate(&args[0], output.as_deref(), |tags| {
                let index = nth_cover(&tags.comments, cover_index).ok_or_else(|| {
                    Error::Invalid(format!("cover index {cover_index} is out of range"))
                })?;
                tags.comments.remove(index);
                Ok(())
            })
        }
        _ => Err(Error::Invalid(format!(
            "unknown command '{command}'\n\n{HELP}"
        ))),
    }
}

fn show(path: &str) -> Result<()> {
    let tags = Tags::read(path)?;
    println!("Vendor: {}", tags.vendor.escape_debug());
    println!("Tags: {}", tags.comments.len());
    let mut cover_index = 0;
    for (index, comment) in tags.comments.iter().enumerate() {
        if comment.name.eq_ignore_ascii_case(PICTURE_TAG) {
            let current_cover = cover_index;
            cover_index += 1;
            match Picture::from_comment(&comment.value) {
                Ok(picture) => {
                    println!(
                        "[{index}] {}=<cover #{current_cover}: type {}, {}, {}x{}, {} bytes, {:?}>",
                        comment.name,
                        picture.picture_type,
                        picture.mime_type.escape_debug(),
                        picture.width,
                        picture.height,
                        picture.data.len(),
                        picture.description
                    );
                }
                Err(error) => println!(
                    "[{index}] {}=<invalid cover #{current_cover}: {error}>",
                    comment.name
                ),
            }
        } else {
            println!(
                "[{index}] {}={}",
                comment.name,
                comment.value.escape_debug()
            );
        }
    }
    Ok(())
}

fn list_covers(path: &str) -> Result<()> {
    let tags = Tags::read(path)?;
    let mut count = 0;
    for (tag_index, comment) in tags.comments.iter().enumerate() {
        if !comment.name.eq_ignore_ascii_case(PICTURE_TAG) {
            continue;
        }
        match Picture::from_comment(&comment.value) {
            Ok(picture) => println!(
                "[{count}] tag={tag_index} type={} mime={} size={}x{} depth={} bytes={} description={:?}",
                picture.picture_type,
                picture.mime_type.escape_debug(),
                picture.width,
                picture.height,
                picture.color_depth,
                picture.data.len(),
                picture.description
            ),
            Err(error) => println!("[{count}] tag={tag_index} invalid={error}"),
        }
        count += 1;
    }
    println!("Covers: {count}");
    Ok(())
}

fn extract_cover(input: &str, cover_index: usize, output: &str) -> Result<()> {
    if paths_refer_to_same_file(Path::new(input), Path::new(output))? {
        return Err(Error::Invalid(
            "cover-extract output must not refer to its input file".into(),
        ));
    }
    let tags = Tags::read(input)?;
    let tag_index = nth_cover(&tags.comments, cover_index)
        .ok_or_else(|| Error::Invalid(format!("cover index {cover_index} is out of range")))?;
    let picture = Picture::from_comment(&tags.comments[tag_index].value)?;
    write_output_file(output, &picture.data)?;
    Ok(())
}

fn nth_cover(comments: &[Comment], wanted: usize) -> Option<usize> {
    comments
        .iter()
        .enumerate()
        .filter(|(_, comment)| comment.name.eq_ignore_ascii_case(PICTURE_TAG))
        .nth(wanted)
        .map(|(index, _)| index)
}

fn mutate(
    input: &str,
    output: Option<&Path>,
    change: impl FnOnce(&mut Tags) -> Result<()>,
) -> Result<()> {
    edit_file(input, output, change)
}

fn paths_refer_to_same_file(input: &Path, output: &Path) -> Result<bool> {
    let input_metadata = fs::metadata(input)?;
    let output_metadata = match fs::metadata(output) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(input_metadata.dev() == output_metadata.dev()
            && input_metadata.ino() == output_metadata.ino())
    }
    #[cfg(not(unix))]
    {
        Ok(fs::canonicalize(input)? == fs::canonicalize(output)?)
    }
}

fn output_option(mut args: Vec<String>) -> Result<(Vec<String>, Option<PathBuf>)> {
    let mut output = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            args.remove(index);
            break;
        } else if matches!(args[index].as_str(), "-o" | "--output") {
            if output.is_some() {
                return Err(Error::Invalid(
                    "output option was supplied more than once".into(),
                ));
            }
            if index + 1 >= args.len() {
                return Err(Error::Invalid("output option requires a file path".into()));
            }
            output = Some(PathBuf::from(args.remove(index + 1)));
            args.remove(index);
        } else {
            index += 1;
        }
    }
    Ok((args, output))
}

fn parse_index(value: &str, kind: &str) -> Result<usize> {
    value
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid {kind} index '{value}'")))
}

fn exact_args(args: &[String], expected: usize, synopsis: &str) -> Result<()> {
    if args.len() != expected {
        return Err(usage(synopsis));
    }
    Ok(())
}

fn usage(synopsis: &str) -> Error {
    Error::Invalid(format!("usage: opustagger {synopsis}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_delimiter_preserves_literal_option_value() {
        let (args, output) = output_option(vec![
            "file.opus".into(),
            "TITLE".into(),
            "--".into(),
            "-o".into(),
        ])
        .unwrap();
        assert_eq!(args, ["file.opus", "TITLE", "-o"]);
        assert_eq!(output, None);
    }

    #[test]
    fn debug_escaping_keeps_unicode_and_escapes_controls() {
        assert_eq!("東京\n\u{1b}".escape_debug().to_string(), "東京\\n\\u{1b}");
    }
}
