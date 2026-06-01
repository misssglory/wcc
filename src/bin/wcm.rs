use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use jwalk::WalkDir;
use lz4_flex::frame::FrameDecoder;
use rayon::prelude::*;

type DynRead = Box<dyn Read + Send>;

fn main() -> io::Result<()> {
    let mut args = env::args().skip(1);

    let needle = match args.next() {
        Some(s) => s,
        None => {
            eprintln!("usage: pfind <needle> [dir]");
            std::process::exit(2);
        }
    };

    let root = args.next().unwrap_or_else(|| ".".to_string());
    let root = PathBuf::from(root);

    let files: Vec<PathBuf> = WalkDir::new(&root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.path())
        .collect();

    files.par_iter().for_each(|path| {
        if let Err(err) = search_file(path, &needle) {
            eprintln!("{}: {}", path.display(), err);
        }
    });

    Ok(())
}

fn search_file(path: &Path, needle: &str) -> io::Result<()> {
    let reader = open_maybe_compressed(path)?;
    let mut reader = BufReader::new(reader);

    let mut line = String::new();
    let mut line_no = 0usize;

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        line_no += 1;

        if line.contains(needle) {
            print_match(path, line_no, &line);
        }
    }

    Ok(())
}

fn open_maybe_compressed(path: &Path) -> io::Result<DynRead> {
    let file = File::open(path)?;

    match path.extension().and_then(|s| s.to_str()) {
        Some("lz4") => Ok(Box::new(FrameDecoder::new(file))),
        Some("gz") => Ok(Box::new(MultiGzDecoder::new(file))),
        _ => Ok(Box::new(file)),
    }
}

fn print_match(path: &Path, line_no: usize, line: &str) {
    print!("{}:{}:{}", path.display(), line_no, line);
    if !line.ends_with('\n') {
        println!();
    }
}