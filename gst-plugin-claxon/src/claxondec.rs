// Copyright (C) 2019 Ruben Gonzalez <rgonzalez@fluendo.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use glib;
use glib::subclass;
use glib::subclass::prelude::*;
use gst;
use gst::subclass::prelude::*;
use gst_audio;
use gst_audio::prelude::*;
use gst_audio::subclass::prelude::*;

use std::io::Cursor;

use atomic_refcell::AtomicRefCell;

use byte_slice_cast::*;

struct State {
    streaminfo: Option<claxon::metadata::StreamInfo>,
    audio_info: Option<gst_audio::AudioInfo>,
}

struct ClaxonDec {
    cat: gst::DebugCategory,
    state: AtomicRefCell<Option<State>>,
}

impl ObjectSubclass for ClaxonDec {
    const NAME: &'static str = "ClaxonDec";
    type ParentType = gst_audio::AudioDecoder;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new() -> Self {
        Self {
            cat: gst::DebugCategory::new(
                "claxondec",
                gst::DebugColorFlags::empty(),
                Some("Claxon FLAC decoder"),
            ),
            state: AtomicRefCell::new(None),
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Claxon FLAC decoder",
            "Decoder/Audio",
            "Claxon FLAC decoder",
            "Ruben Gonzalez <rgonzalez@fluendo.com>",
        );

        let sink_caps = gst::Caps::new_simple("audio/x-flac", &[("framed", &true)]);
        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &sink_caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let src_caps = gst::Caps::new_simple(
            "audio/x-raw",
            &[
                (
                    "format",
                    &gst::List::new(&[
                        &gst_audio::AudioFormat::S8.to_str(),
                        &gst_audio::AUDIO_FORMAT_S16.to_str(),
                        &gst_audio::AUDIO_FORMAT_S2432.to_str(),
                        &gst_audio::AUDIO_FORMAT_S32.to_str(),
                    ]),
                ),
                ("rate", &gst::IntRange::<i32>::new(1, 655_350)),
                ("channels", &gst::IntRange::<i32>::new(1, 8)),
                ("layout", &"interleaved"),
            ],
        );
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &src_caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);
    }
}

impl ObjectImpl for ClaxonDec {
    glib_object_impl!();
}

impl ElementImpl for ClaxonDec {}

impl AudioDecoderImpl for ClaxonDec {
    fn stop(&self, _element: &gst_audio::AudioDecoder) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = None;

        Ok(())
    }

    fn start(&self, _element: &gst_audio::AudioDecoder) -> Result<(), gst::ErrorMessage> {
        *self.state.borrow_mut() = Some(State {
            streaminfo: None,
            audio_info: None,
        });

        Ok(())
    }

    fn set_format(
        &self,
        element: &gst_audio::AudioDecoder,
        caps: &gst::Caps,
    ) -> Result<(), gst::LoggableError> {
        gst_debug!(self.cat, obj: element, "Setting format {:?}", caps);

        let mut streaminfo: Option<claxon::metadata::StreamInfo> = None;
        let mut audio_info: Option<gst_audio::AudioInfo> = None;

        let s = caps.get_structure(0).unwrap();
        if let Ok(Some(streamheaders)) = s.get_optional::<gst::Array>("streamheader") {
            let streamheaders = streamheaders.as_slice();

            if streamheaders.len() < 2 {
                gst_debug!(
                    self.cat,
                    obj: element,
                    "Not enough streamheaders, trying in-band"
                );
            } else {
                let ident_buf = streamheaders[0].get::<gst::Buffer>();
                if let Ok(Some(ident_buf)) = ident_buf {
                    gst_debug!(self.cat, obj: element, "Got streamheader buffers");
                    let inmap = ident_buf.map_readable().unwrap();

                    if inmap[0..7] != [0x7f, b'F', b'L', b'A', b'C', 0x01, 0x00] {
                        gst_debug!(self.cat, obj: element, "Unknown streamheader format");
                    } else if let Ok(tstreaminfo) = get_claxon_streaminfo(&inmap[13..]) {
                        if let Ok(taudio_info) = get_gstaudioinfo(tstreaminfo) {
                            // To speed up negotiation
                            if element.set_output_format(&taudio_info).is_err()
                                || element.negotiate().is_err()
                            {
                                gst_debug!(
                                    self.cat,
                                    obj: element,
                                    "Error to negotiate output from based on in-caps streaminfo"
                                );
                            }

                            audio_info = Some(taudio_info);
                            streaminfo = Some(tstreaminfo);
                        }
                    }
                }
            }
        }

        let mut state_guard = self.state.borrow_mut();
        *state_guard = Some(State {
            streaminfo,
            audio_info,
        });

        Ok(())
    }

    #[allow(clippy::verbose_bit_mask)]
    fn handle_frame(
        &self,
        element: &gst_audio::AudioDecoder,
        inbuf: Option<&gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_debug!(self.cat, obj: element, "Handling buffer {:?}", inbuf);

        let inbuf = match inbuf {
            None => return Ok(gst::FlowSuccess::Ok),
            Some(inbuf) => inbuf,
        };

        let inmap = inbuf.map_readable().map_err(|_| {
            gst_error!(self.cat, obj: element, "Failed to buffer readable");
            gst::FlowError::Error
        })?;

        let mut state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().ok_or(gst::FlowError::NotNegotiated)?;

        if inmap.as_slice() == b"fLaC" {
            gst_debug!(self.cat, obj: element, "fLaC buffer received");
        } else if inmap[0] & 0x7F == 0x00 {
            gst_debug!(self.cat, obj: element, "Streaminfo header buffer received");
            return self.handle_streaminfo_header(element, state, inmap.as_ref());
        } else if inmap[0] == 0b1111_1111 && inmap[1] & 0b1111_1100 == 0b1111_1000 {
            gst_debug!(self.cat, obj: element, "Data buffer received");
            return self.handle_data(element, state, inmap.as_ref());
        } else {
            // info about other headers in flacparse and https://xiph.org/flac/format.html
            gst_debug!(
                self.cat,
                obj: element,
                "Other header buffer received {:?}",
                inmap[0] & 0x7F
            );
        }

        element.finish_frame(None, 1)
    }
}

impl ClaxonDec {
    fn handle_streaminfo_header(
        &self,
        element: &gst_audio::AudioDecoder,
        state: &mut State,
        indata: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let streaminfo = match get_claxon_streaminfo(indata) {
            Ok(v) => v,
            Err(error) => {
                gst_element_error!(element, gst::StreamError::Decode, [error]);
                return Err(gst::FlowError::Error);
            }
        };

        let audio_info = match get_gstaudioinfo(streaminfo) {
            Ok(v) => v,
            Err(error) => {
                gst_element_error!(element, gst::StreamError::Decode, [error]);
                return Err(gst::FlowError::Error);
            }
        };

        gst_debug!(
            self.cat,
            obj: element,
            "Successfully parsed headers: {:?}",
            audio_info
        );

        element.set_output_format(&audio_info)?;
        element.negotiate()?;

        state.streaminfo = Some(streaminfo);
        state.audio_info = Some(audio_info);

        element.finish_frame(None, 1)
    }

    fn handle_data(
        &self,
        element: &gst_audio::AudioDecoder,
        state: &mut State,
        indata: &[u8],
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        // TODO It's valid for FLAC to not have any streaminfo header at all, for a small subset
        // of possible FLAC configurations. (claxon does not actually support that)
        let audio_info = state
            .audio_info
            .as_ref()
            .ok_or(gst::FlowError::NotNegotiated)?;
        let channels = audio_info.channels() as usize;

        if channels > 8 {
            unreachable!(
                "FLAC only supports from 1 to 8 channels (audio contains {} channels)",
                channels
            );
        }

        if ![8, 16, 24, 32].contains(&audio_info.depth()) {
            unreachable!(
                "claxondec doesn't supports {}bits audio",
                audio_info.depth()
            );
        }

        let buffer = Vec::new();
        let mut cursor = Cursor::new(indata);
        let mut reader = claxon::frame::FrameReader::new(&mut cursor);
        let result = match reader.read_next_or_eof(buffer) {
            Ok(Some(result)) => result,
            Ok(None) => return element.finish_frame(None, 1),
            Err(err) => {
                return gst_audio_decoder_error!(
                    element,
                    1,
                    gst::StreamError::Decode,
                    ["Failed to decode packet: {:?}", err]
                );
            }
        };

        assert_eq!(cursor.position(), indata.len() as u64);

        let v = if channels != 1 {
            let mut v: Vec<i32> = vec![0; result.len() as usize];

            for (o, i) in v.chunks_exact_mut(channels).enumerate() {
                for (c, s) in i.iter_mut().enumerate() {
                    *s = result.sample(c as u32, o as u32);
                }
            }
            v
        } else {
            result.into_buffer()
        };

        let outbuf = if audio_info.depth() == 8 {
            let v = v.iter().map(|e| *e as i8).collect::<Vec<_>>();
            gst::Buffer::from_slice(v.into_byte_vec())
        } else if audio_info.depth() == 16 {
            let v = v.iter().map(|e| *e as i16).collect::<Vec<_>>();
            gst::Buffer::from_slice(v.into_byte_vec())
        } else {
            gst::Buffer::from_slice(v.into_byte_vec())
        };

        element.finish_frame(Some(outbuf), 1)
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "claxondec",
        gst::Rank::Marginal,
        ClaxonDec::get_type(),
    )
}

fn get_claxon_streaminfo(indata: &[u8]) -> Result<claxon::metadata::StreamInfo, &'static str> {
    let mut cursor = Cursor::new(indata);
    let mut metadata_iter = claxon::metadata::MetadataBlockReader::new(&mut cursor);
    let streaminfo = match metadata_iter.next() {
        Some(Ok(claxon::metadata::MetadataBlock::StreamInfo(info))) => info,
        _ => return Err("Failed to decode STREAMINFO"),
    };

    assert_eq!(cursor.position(), indata.len() as u64);

    Ok(streaminfo)
}

fn get_gstaudioinfo(
    streaminfo: claxon::metadata::StreamInfo,
) -> Result<gst_audio::AudioInfo, &'static str> {
    let format = match streaminfo.bits_per_sample {
        8 => gst_audio::AudioFormat::S8,
        16 => gst_audio::AUDIO_FORMAT_S16,
        24 => gst_audio::AUDIO_FORMAT_S2432,
        32 => gst_audio::AUDIO_FORMAT_S32,
        _ => return Err("format not supported"),
    };

    if streaminfo.channels > 8 {
        return Err("more than 8 channels not supported yet");
    }
    let mut audio_info =
        gst_audio::AudioInfo::new(format, streaminfo.sample_rate, streaminfo.channels);

    let index = streaminfo.channels as usize;
    let to = &FLAC_CHANNEL_POSITIONS[index - 1][..index];
    audio_info = audio_info.positions(to);

    Ok(audio_info.build().unwrap())
}

// http://www.xiph.org/vorbis/doc/Vorbis_I_spec.html#x1-800004.3.9
// http://flac.sourceforge.net/format.html#frame_header
const FLAC_CHANNEL_POSITIONS: [[gst_audio::AudioChannelPosition; 8]; 8] = [
    [
        gst_audio::AudioChannelPosition::Mono,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::Invalid,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    // FIXME: 7/8 channel layouts are not defined in the FLAC specs
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::RearCenter,
        gst_audio::AudioChannelPosition::Lfe1,
        gst_audio::AudioChannelPosition::Invalid,
    ],
    [
        gst_audio::AudioChannelPosition::FrontLeft,
        gst_audio::AudioChannelPosition::FrontCenter,
        gst_audio::AudioChannelPosition::FrontRight,
        gst_audio::AudioChannelPosition::SideLeft,
        gst_audio::AudioChannelPosition::SideRight,
        gst_audio::AudioChannelPosition::RearLeft,
        gst_audio::AudioChannelPosition::RearRight,
        gst_audio::AudioChannelPosition::Lfe1,
    ],
];
