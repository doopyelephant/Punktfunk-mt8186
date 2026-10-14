//! The decode order, pinned as one table: for each OS, GPU and stored preference the
//! ladder tells apart, the rungs [`Decoder::new`](crate::video::Decoder::new) tries
//! and where a rung that keeps failing demotes. `build_rung` asks [`tests::scripted`]
//! first, so the walk runs with no decoder behind it.

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use anyhow::anyhow;
    use anyhow::Result;
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};

    use crate::video::{
        usable_decode_ops, DecodeHealth, DecodedImage, Decoder, NativeRung, NoSoftwareRung,
        StreamFormat, VulkanDecodeDevice, VIDEO_CODEC_OP_DECODE_AV1, VIDEO_CODEC_OP_DECODE_H264,
        VIDEO_CODEC_OP_DECODE_H265,
    };
    use crate::video_caps::tests::decode_device;
    use crate::video_rung::Rung;

    /// Columns: OS; GPU vendor (`none` is no presenter device); the codecs its Vulkan
    /// Video advertises (`all`, `h26x` without AV1, `none`); whether the presenter
    /// imports the platform rung's pictures (dma-buf, D3D11 texture); wire codec;
    /// stored preference (`empty` is `""`). Then the rungs tried in order, and for
    /// each hardware rung the session can land on, the rungs it demotes through.
    /// Every demotion has one hardware target at most: if that does not build, the
    /// CPU rung is next.
    const ORDER: &str = "
        os      gpu    vulkan import codec pref           | tries                            | a failing rung demotes through
        linux   nvidia all    yes    h264  auto           | vulkan v4l2 software             | vulkan>software v4l2>software
        linux   amd    all    yes    h264  auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    hevc  auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    h26x   yes    av1   auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   intel  all    yes    h264  auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   intel  all    yes    av1   auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   other  all    yes    av1   auto           | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   intel  none   yes    h264  auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   nvidia none   yes    h264  auto           | v4l2 software                    | v4l2>software
        linux   amd    all    no     h264  auto           | vulkan software                  | vulkan>software
        linux   amd    none   no     h264  auto           | software                         | -
        linux   none   -      -      h264  auto           | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   amd    all    yes    h264  hardware       | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    h264  empty          | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    h264  software       | software                         | -
        linux   amd    all    yes    h264  native-vulkan  | vulkan vaapi v4l2 software       | vulkan>vaapi>software vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   intel  all    yes    av1   native-vulkan  | vaapi v4l2 software              | vaapi>software v4l2>vaapi>software
        linux   nvidia all    yes    h264  native-vaapi   | vaapi vulkan v4l2 software       | vaapi>vulkan>software vulkan>software v4l2>software
        linux   amd    all    yes    h264  native-vaapi   | vaapi vulkan vaapi v4l2 software | vaapi>vulkan>software vulkan>vaapi>software v4l2>vaapi>vulkan>software
        linux   intel  all    yes    av1   native-vaapi   | vaapi vaapi v4l2 software        | vaapi>software v4l2>vaapi>software
        linux   amd    all    yes    h264  native-v4l2    | v4l2 vulkan vaapi v4l2 software  | v4l2>vaapi>vulkan>software vulkan>vaapi>software vaapi>vulkan>software
        linux   amd    all    yes    h264  something-else | vaapi v4l2 software              | vaapi>vulkan>software v4l2>vaapi>vulkan>software
        linux   amd    all    yes    h264  native-d3d11va | vaapi v4l2 software              | vaapi>vulkan>software v4l2>vaapi>vulkan>software
        windows nvidia all    yes    h264  auto           | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows amd    all    yes    h264  auto           | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows amd    h26x   yes    av1   auto           | d3d11va software                 | d3d11va>software
        windows intel  all    yes    h264  auto           | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows intel  all    yes    av1   auto           | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows other  all    yes    h264  auto           | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>d3d11va>software
        windows nvidia none   yes    h264  auto           | d3d11va software                 | d3d11va>software
        windows nvidia all    no     h264  auto           | vulkan software                  | vulkan>software
        windows intel  all    no     h264  auto           | vulkan software                  | vulkan>software
        windows none   -      -      h264  auto           | software                         | -
        windows nvidia all    yes    h264  hardware       | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows nvidia all    yes    h264  software       | software                         | -
        windows intel  all    yes    h264  native-vulkan  | vulkan d3d11va software          | vulkan>d3d11va>software d3d11va>vulkan>software
        windows amd    h26x   yes    av1   native-vulkan  | d3d11va software                 | d3d11va>software
        windows nvidia all    yes    h264  native-d3d11va | d3d11va vulkan d3d11va software  | d3d11va>vulkan>software vulkan>d3d11va>software
        windows intel  all    yes    h264  native-d3d11va | d3d11va d3d11va vulkan software  | d3d11va>vulkan>software vulkan>d3d11va>software
        windows nvidia all    no     h264  native-d3d11va | d3d11va vulkan software          | d3d11va>vulkan>software vulkan>software
        windows nvidia all    yes    h264  something-else | d3d11va software                 | d3d11va>vulkan>software
        windows nvidia all    yes    h264  native-vaapi   | d3d11va software                 | d3d11va>vulkan>software
    ";

    struct Row {
        os: &'static str,
        gpu: &'static str,
        vulkan: &'static str,
        import: &'static str,
        codec: &'static str,
        pref: &'static str,
        tries: &'static str,
        demotes: &'static str,
    }

    impl Row {
        fn wire(&self) -> u8 {
            match self.codec {
                "h264" => CODEC_H264,
                "hevc" => CODEC_HEVC,
                "av1" => CODEC_AV1,
                other => panic!("codec {other}"),
            }
        }

        fn pref(&self) -> &'static str {
            match self.pref {
                "empty" => "",
                other => other,
            }
        }

        /// The presenter's device as the ladder sees it: the advertised ops already
        /// masked, as the presenter masks them.
        fn device(&self) -> Option<VulkanDecodeDevice> {
            let vendor = match self.gpu {
                "nvidia" => 0x10DE,
                "amd" => 0x1002,
                "intel" => 0x8086,
                "other" => 0x5143,
                "none" => return None,
                other => panic!("gpu {other}"),
            };
            let h26x = VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265;
            let ops = match self.vulkan {
                "all" => h26x | VIDEO_CODEC_OP_DECODE_AV1,
                "h26x" => h26x,
                "none" => 0,
                other => panic!("vulkan {other}"),
            };
            let mut vk = decode_device(vendor, self.gpu);
            vk.video_decode = ops != 0;
            vk.decode_video_caps = usable_decode_ops(vendor, ops);
            vk.dmabuf_import = self.import == "yes" && self.os == "linux";
            vk.d3d11_import = self.import == "yes" && self.os == "windows";
            Some(vk)
        }

        fn label(&self) -> String {
            format!(
                "{} {} {} {} {} {}",
                self.os, self.gpu, self.vulkan, self.import, self.codec, self.pref
            )
        }
    }

    fn rows() -> Vec<Row> {
        let lines = ORDER.lines().map(str::trim).filter(|l| !l.is_empty());
        lines
            .skip(1)
            .map(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                let facts: Vec<&str> = cells[0].split_whitespace().collect();
                assert_eq!((cells.len(), facts.len()), (3, 6), "{line}");
                Row {
                    os: facts[0],
                    gpu: facts[1],
                    vulkan: facts[2],
                    import: facts[3],
                    codec: facts[4],
                    pref: facts[5],
                    tries: cells[1],
                    demotes: cells[2],
                }
            })
            .collect()
    }

    /// Rungs as a table cell, the CPU rung last: it follows every walk.
    fn cell(rungs: &[NativeRung], between: &str) -> String {
        let hardware = rungs.iter().map(|r| r.name().trim_start_matches("native-"));
        let all: Vec<&str> = hardware.chain(["software"]).collect();
        all.join(between)
    }

    /// What `build_rung` does while a walk is scripted: it records every rung asked
    /// for, refuses the first `refuse`, and lands the rest on a [`Failing`] rung.
    struct Script {
        tried: Vec<NativeRung>,
        refuse: usize,
    }

    thread_local! {
        static SCRIPT: RefCell<Option<Script>> = const { RefCell::new(None) };
    }

    /// `build_rung`'s answer under a script. `None` builds the real rung.
    pub(crate) fn scripted(rung: NativeRung) -> Option<Result<Box<dyn Rung>>> {
        SCRIPT.with_borrow_mut(|script| {
            let script = script.as_mut()?;
            script.tried.push(rung);
            if script.refuse > 0 {
                script.refuse -= 1;
                return Some(Err(anyhow!("scripted refusal")));
            }
            Some(Ok(Box::new(Failing(rung)) as Box<dyn Rung>))
        })
    }

    fn take_tried() -> Vec<NativeRung> {
        SCRIPT.with_borrow_mut(|s| std::mem::take(&mut s.as_mut().expect("scripted").tried))
    }

    /// A rung that refuses every access unit as a device fact, which demotes at once.
    struct Failing(NativeRung);

    impl Rung for Failing {
        fn id(&self) -> Option<NativeRung> {
            Some(self.0)
        }

        fn name(&self) -> &'static str {
            self.0.name()
        }

        fn decode(&mut self, _: &[u8], _: u32, _: bool) -> Result<Option<DecodedImage>> {
            let refused = pf_vkdecode::CapsError::SliceSegments { segments: 2 };
            Err(pf_vkdecode::VkDecodeError::Caps(refused).into())
        }

        // The CPU rung reports none, so `Some` tells the two apart.
        fn health(&self) -> Option<DecodeHealth> {
            Some(DecodeHealth::default())
        }
    }

    /// `Decoder::new` for `row`, with the first `refuse` hardware builds refused.
    fn start(row: &Row, refuse: usize) -> (Result<Decoder>, Vec<NativeRung>) {
        SCRIPT.set(Some(Script {
            tried: Vec::new(),
            refuse,
        }));
        let vk = row.device();
        let decoder = Decoder::new(row.wire(), row.pref(), vk.as_ref(), StreamFormat::SDR_420_8);
        (decoder, take_tried())
    }

    /// The walk ended on the CPU rung: it runs, or refused a codec it has no decoder for.
    fn on_cpu_rung(row: &Row, landed: std::result::Result<&Decoder, &anyhow::Error>) -> bool {
        match landed {
            Ok(d) => d.decode_health().is_none(),
            Err(e) => row.codec == "hevc" && e.downcast_ref::<NoSoftwareRung>().is_some(),
        }
    }

    /// Land on the `step`th rung `row` tries, then fail every access unit: the rungs
    /// the session demotes through. `refuse_targets` refuses every demotion build.
    fn demotes(row: &Row, step: usize, refuse_targets: bool) -> Vec<NativeRung> {
        let (decoder, tried) = start(row, step);
        let mut decoder = decoder.expect("the scripted rung lands");
        let mut chain = vec![*tried.last().expect("a rung landed")];
        if refuse_targets {
            SCRIPT.with_borrow_mut(|s| s.as_mut().expect("scripted").refuse = usize::MAX);
        }
        loop {
            let unit = decoder.decode(&[]);
            let built = take_tried();
            if built.is_empty() || refuse_targets {
                let landed = unit.as_ref().map(|_| &decoder);
                assert!(on_cpu_rung(row, landed), "{}", row.label());
                chain.extend(built);
                return chain;
            }
            assert_eq!(built.len(), 1, "{}: one target lands", row.label());
            chain.extend(built);
        }
    }

    /// The table holds for the walk this build runs: what `Decoder::new` tries, and
    /// where each rung it can land on goes when it keeps failing.
    #[test]
    fn the_decoder_walks_the_order_table() {
        // `Decoder::new` reads the override first: a set one answers for every row.
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        let host = if cfg!(windows) { "windows" } else { "linux" };
        let table = rows();
        let here: Vec<&Row> = table.iter().filter(|r| r.os == host).collect();
        assert!(here.len() > 10, "the table lost its {host} rows");
        for row in here {
            let (landed, tried) = start(row, usize::MAX);
            assert!(on_cpu_rung(row, landed.as_ref()), "{}", row.label());
            assert_eq!(cell(&tried, " "), row.tries, "{}", row.label());

            let mut chains = Vec::new();
            for (step, rung) in tried.iter().enumerate() {
                if tried[..step].contains(rung) {
                    continue;
                }
                let chain = demotes(row, step, false);
                // A target that does not build leaves only the CPU rung.
                let refused = demotes(row, step, true);
                assert_eq!(refused[..], chain[..chain.len().min(2)], "{}", row.label());
                chains.push(cell(&chain, ">"));
            }
            let demotes = if chains.is_empty() {
                "-".to_string()
            } else {
                chains.join(" ")
            };
            assert_eq!(demotes, row.demotes, "{}", row.label());
        }
    }
}
