//! Saving recordings as MP4 files.
//!
//! Two sources end up here. A recording an NVR serves over HTTP is already a
//! container, and is copied into an MP4 as it is. A recording replayed over
//! Baichuan arrives as bare H.264/H.265 frames, which are written to a raw
//! elementary-stream file first and then copied into an MP4 the same way —
//! ffmpeg's raw demuxer works out the codec parameters an MP4 needs, and the
//! frames are given even timestamps at the rate the recording ran at.
//!
//! Neither path re-encodes: the picture is the device's own.

use std::path::Path;

use ffmpeg_next as ffmpeg;

/// Copy whatever `input` (a URL or a file path) holds into an MP4 at `out`.
///
/// `fps`, when given, sets the raw demuxer's frame rate and gives every packet a
/// timestamp at that rate — for an elementary stream, which carries none of its
/// own. A container input keeps the timestamps it has.
pub fn to_mp4(input: &str, fps: Option<f64>, out: &Path) -> Result<(), String> {
    ffmpeg::init().map_err(|e| e.to_string())?;
    let mut options = ffmpeg::Dictionary::new();
    if let Some(fps) = fps {
        options.set("framerate", &format!("{fps:.3}"));
    }
    let mut ictx = ffmpeg::format::input_with_dictionary(&input, options).map_err(|e| format!("{input}: {e}"))?;
    let mut octx = ffmpeg::format::output_as(&out, "mp4").map_err(|e| format!("{}: {e}", out.display()))?;

    // Video (and audio, when a container has it) only, each to a stream of its own.
    let mut mapping: Vec<Option<usize>> = vec![None; ictx.nb_streams() as usize];
    for (index, stream) in ictx.streams().enumerate() {
        let medium = stream.parameters().medium();
        if medium != ffmpeg::media::Type::Video && medium != ffmpeg::media::Type::Audio {
            continue;
        }
        let mut ost = octx
            .add_stream(ffmpeg::encoder::find(ffmpeg::codec::Id::None))
            .map_err(|e| e.to_string())?;
        ost.set_parameters(stream.parameters());
        // Let the MP4 muxer choose the tag; a raw stream's would not suit it.
        unsafe {
            (*ost.parameters().as_mut_ptr()).codec_tag = 0;
        }
        mapping[index] = Some(ost.index());
    }
    if mapping.iter().all(Option::is_none) {
        return Err("no video in the recording".into());
    }
    octx.write_header().map_err(|e| e.to_string())?;

    let frame_step = fps.map(|fps| 1.0 / fps);
    let mut written = 0i64;
    for (stream, mut packet) in ictx.packets() {
        let Some(out_index) = mapping[stream.index()] else { continue };
        let out_base = octx.stream(out_index).ok_or("output stream vanished")?.time_base();
        match frame_step {
            // An elementary stream: time each frame by its place in the stream.
            Some(step) => {
                let at = (written as f64 * step / f64::from(out_base)).round() as i64;
                packet.set_pts(Some(at));
                packet.set_dts(Some(at));
                packet.set_duration((step / f64::from(out_base)).round() as i64);
                written += 1;
            }
            None => packet.rescale_ts(stream.time_base(), out_base),
        }
        packet.set_position(-1);
        packet.set_stream(out_index);
        packet.write_interleaved(&mut octx).map_err(|e| e.to_string())?;
    }
    octx.write_trailer().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raw H.264 stream becomes an MP4 a player can open, at the rate asked for:
    /// its duration is its frame count over that rate.
    #[test]
    fn an_elementary_stream_becomes_an_mp4_at_its_frame_rate() {
        let dir = std::env::temp_dir().join(format!("kestrel-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let es = dir.join("clip.h264");
        std::fs::write(&es, include_bytes!("testdata/synthetic_h264.bin")).unwrap();
        let mp4 = dir.join("clip.mp4");

        to_mp4(es.to_str().unwrap(), Some(10.0), &mp4).expect("remux");

        let input = ffmpeg::format::input(&mp4).expect("the MP4 opens");
        let video = input.streams().best(ffmpeg::media::Type::Video).expect("a video stream");
        let frames = video.frames();
        let seconds = input.duration() as f64 / f64::from(ffmpeg::ffi::AV_TIME_BASE);
        assert!(frames > 0, "frames were written");
        assert!(
            (seconds - frames as f64 / 10.0).abs() < 0.25,
            "{frames} frames at 10 fps should last ~{}s, got {seconds}s",
            frames as f64 / 10.0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
