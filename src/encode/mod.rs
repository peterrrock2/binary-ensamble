//! This module contains the main encoding functions for turning an
//! input JSONL or BEN file into a BEN or XBEN file.
//!
//! Any input JSONL file is expected to be in the standard
//!
//! ```json
//! {"assignment": [...], "sample": #}
//! ```
//!
//! format.
//!
//! The BEN format is
//! a simple bit-packed run-length encoded assignment vector with
//! some special headers that allow the decoder to know how many
//! bytes to read for each sample.
//!
//!
//! The XBEN format uses LZMA2 dictionary compression on
//! a byte-level decompressed version of the BEN format (known as ben32)
//! to achieve better compression ratios than we could achieve with applying
//! LZMA2 compression directly to the BEN format.

pub mod relabel;
pub mod translate;

use crate::utils::*;
use serde_json::Value;
use std::io::{self, BufRead, Write};
use xz2::write::XzEncoder;

use self::translate::ben_to_ben32_lines;

/// This function takes a json encoded line containing an assignment
/// vector and a sample number and encodes the assignment vector
/// into a binary format known as "ben32". The ben32 format serves
/// as an intermediate format that allows for efficient compression
/// of BEN files using LZMA2 compression methods.
///
/// # Arguments
///
/// * `data` - A JSON object containing an assignment vector and a sample number
///
/// # Returns
///
/// A vector of bytes containing the ben32 encoded assignment vector
fn encode_ben_32_line(data: Value) -> Vec<u8> {
    let assign_vec = data["assignment"].as_array().unwrap();
    let mut prev_assign: u16 = 0;
    let mut count: u16 = 0;
    let mut first = true;

    let mut ret = Vec::new();

    for assignment in assign_vec {
        let assign = assignment.as_u64().unwrap() as u16;
        if first {
            prev_assign = assign;
            count = 1;
            first = false;
            continue;
        }
        if assign == prev_assign {
            count += 1;
        } else {
            let encoded = (prev_assign as u32) << 16 | count as u32;
            ret.extend(&encoded.to_be_bytes());
            // Reset for next run
            prev_assign = assign;
            count = 1;
        }
    }

    // Handle the last run
    if count > 0 {
        let encoded = (prev_assign as u32) << 16 | count as u32;
        ret.extend(&encoded.to_be_bytes());
    }

    ret.extend([0, 0, 0, 0]);
    ret
}

/// This function takes a JSONL file and compresses it to the
/// XBEN format.
///
/// The JSONL file is assumed to be formatted in the standard
///
/// ```json
/// {"assignment": [...], "sample": #}
/// ```
///
/// format. While the BEN format is
/// a simple bit-packed (streamable!) run-length encoded assignment
/// vector, the XBEN format uses LZMA2 dictionary compression on
/// the byte level to achieve better compression ratios. In order
/// to use XBEN files, the `decode_xben_to_ben` function must be
/// used to decode the file back into a BEN format.
pub fn jsonl_encode_xben<R: BufRead, W: Write>(reader: R, mut writer: W) -> std::io::Result<()> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut encoder = XzEncoder::new(&mut buffer, 9);

    let mut line_num = 1;

    encoder.write_all("STANDARD BEN FILE".as_bytes())?;
    for line_result in reader.lines() {
        print!("Encoding line: {}\r", line_num);
        line_num += 1;
        let line = line_result?;
        let data: Value = serde_json::from_str(&line).expect("Error parsing JSON from line");

        let ben32_vec = encode_ben_32_line(data);
        encoder.write_all(&ben32_vec)?;
    }
    drop(encoder); // Make sure to flush and finish compression
    writer.write_all(&buffer)?;
    eprintln!();
    eprintln!("Done!");
    Ok(())
}

/// This is a convenience function that applies level 9 LZMA2 compression
/// to a general file.
///
/// # Arguments
///
/// * `reader` - A buffered reader for the input file
/// * `writer` - A writer for the output file
///
/// # Returns
///
/// A Result type that contains the result of the operation
///
/// ```
/// use ben::encode::xz_compress;
/// use lipsum::lipsum;
/// use std::io::{BufReader, BufWriter};
///
/// let input = lipsum(100);
/// let reader = BufReader::new(input.as_bytes());
///
/// let mut output_buffer = Vec::new();
/// let writer = BufWriter::new(&mut output_buffer);
///
/// xz_compress(reader, writer).unwrap();
///
/// println!("{:?}", output_buffer);
/// ```
pub fn xz_compress<R: BufRead, W: Write>(mut reader: R, writer: W) -> std::io::Result<()> {
    let mut buff = [0; 4096];
    let mut encoder = XzEncoder::new(writer, 9);

    while let Ok(count) = reader.read(&mut buff) {
        if count == 0 {
            break;
        }
        encoder.write_all(&buff[..count])?;
    }
    drop(encoder); // Make sure to flush and finish compression
    Ok(())
}

/// This function takes a run-length encoded assignment vector and
/// encodes into a bit-packed ben version
///
/// # Arguments
///
/// * `rle_vec` - A vector of tuples containing the value and length of each run
///
/// # Returns
///
/// A vector of bytes containing the bit-packed ben encoded assignment vector
fn encode_ben_vec_from_rle(rle_vec: Vec<(u16, u16)>) -> Vec<u8> {
    let mut output_vec: Vec<u8> = Vec::new();

    let max_val: u16 = rle_vec.iter().max_by_key(|x| x.0).unwrap().0;
    let max_len: u16 = rle_vec.iter().max_by_key(|x| x.1).unwrap().1;
    let max_val_bits: u8 = (16 - max_val.leading_zeros() as u8).max(1);
    let max_len_bits: u8 = 16 - max_len.leading_zeros() as u8;
    let assign_bits: u32 = (max_val_bits + max_len_bits) as u32;
    let n_bytes: u32 = if (assign_bits * rle_vec.len() as u32) % 8 == 0 {
        (assign_bits * rle_vec.len() as u32) / 8
    } else {
        (assign_bits * rle_vec.len() as u32) / 8 + 1
    };

    output_vec.push(max_val_bits);
    output_vec.push(max_len_bits);
    output_vec.extend(n_bytes.to_be_bytes().as_slice());

    let mut remainder: u32 = 0;
    let mut remainder_bits: u8 = 0;

    for (val, len) in rle_vec {
        let mut new_val: u32 = (remainder << max_val_bits) | (val as u32);

        let mut buff: u8;

        let mut n_bits_left: u8 = remainder_bits + max_val_bits;

        while n_bits_left >= 8 {
            n_bits_left -= 8;
            buff = (new_val >> n_bits_left) as u8;
            output_vec.push(buff);
            new_val = new_val & (!((0xFFFFFFFF as u32) << n_bits_left));
        }

        new_val = (new_val << max_len_bits) | (len as u32);
        n_bits_left += max_len_bits;

        while n_bits_left >= 8 {
            n_bits_left -= 8;
            buff = (new_val >> n_bits_left) as u8;
            output_vec.push(buff);
            new_val = new_val & (!((0xFFFFFFFF as u32) << n_bits_left));
        }

        remainder_bits = n_bits_left;
        remainder = new_val;
    }

    if remainder_bits > 0 {
        let buff = (remainder << (8 - remainder_bits)) as u8;
        output_vec.push(buff);
    }

    output_vec
}

/// This function takes a JSONL file and compresses it into
/// the BEN format.
///
/// The JSONL file is assumed to be formatted in the standard
///
/// ```json
/// {"assignment": [...], "sample": #}
/// ```
///
/// format.
///
/// # Arguments
///
/// * `reader` - A buffered reader for the input file
/// * `writer` - A writer for the output file
///
/// # Returns
///
/// A Result type that contains the result of the operation
///
/// # Example
///
/// ```
/// use std::io::{BufReader, BufWriter};
/// use serde_json::json;
/// use ben::encode::jsonl_encode_ben;
///
/// let input = r#"{"assignment": [1,1,1,2,2,2], "sample": 1}"#.to_string()
///     + "\n"
///     + r#"{"assignment": [1,1,2,2,1,2], "sample": 2}"#;
///
/// let reader = BufReader::new(input.as_bytes());
/// let mut write_buffer = Vec::new();
/// let mut writer = BufWriter::new(&mut write_buffer);
///
/// jsonl_encode_ben(reader, writer).unwrap();
///
/// println!("{:?}", write_buffer);
/// // This will output
/// // [83, 84, 65, 78, 68, 65, 82, 68, 32,
/// //  66, 69, 78, 32, 70, 73, 76, 69, 2,
/// //  2, 0, 0, 0, 1, 123, 2, 2, 0, 0, 0,
/// //  2, 106, 89]
/// ```
///
pub fn jsonl_encode_ben<R: BufRead, W: Write>(reader: R, mut writer: W) -> std::io::Result<()> {
    let mut line_num = 1;
    writer.write_all("STANDARD BEN FILE".as_bytes())?;
    for line_result in reader.lines() {
        print!("Encoding line: {}\r", line_num);
        line_num += 1;
        let line = line_result?; // Handle potential I/O errors for each line
        let data: Value = serde_json::from_str(&line).expect("Error parsing JSON from line");

        if let Some(assign_vec) = data["assignment"].as_array() {
            let rle_vec: Vec<(u16, u16)> = assign_to_rle(
                assign_vec
                    .into_iter()
                    .map(|x| x.as_u64().unwrap() as u16)
                    .collect(),
            );

            let encoded = encode_ben_vec_from_rle(rle_vec);
            writer.write_all(&encoded)?;
        }
    }
    eprintln!();
    eprintln!("Done!"); // Print newline after progress bar
    Ok(())
}

/// This function takes a BEN file and encodes it into an XBEN
/// file using bit-to-byte decompression followed by LZMA2 compression.
///
/// # Arguments
///
/// * `reader` - A buffered reader for the input file
/// * `writer` - A writer for the output file
///
/// # Returns
///
/// A Result type that contains the result of the operation
pub fn encode_ben_to_xben<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
) -> std::io::Result<()> {
    let mut check_buffer = [0u8; 17];
    reader.read_exact(&mut check_buffer)?;

    if &check_buffer != b"STANDARD BEN FILE" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid file format",
        ));
    }

    let mut buffer: Vec<u8> = Vec::new();
    let mut encoder = XzEncoder::new(&mut buffer, 9);

    encoder.write_all(b"STANDARD BEN FILE")?;

    ben_to_ben32_lines(reader, &mut encoder)?;

    drop(encoder); // Make sure to flush and finish compression
    writer.write_all(&buffer)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    include!("tests/encode_tests.rs");
}