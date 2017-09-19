// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::sync::Mutex;

use std::u32;
use std::u64;
use std::collections::BTreeMap;

use error::*;
use element::*;

use glib;
use gst;
use gst::prelude::*;
use gst_base;
use gst_base::prelude::*;

pub type StreamIndex = u32;

#[derive(Debug)]
pub enum SeekResult {
    TooEarly,
    Ok(u64),
    Eos,
}

#[derive(Debug)]
pub enum HandleBufferResult {
    NeedMoreData,
    Again,
    // NeedDataFromOffset(u64),
    StreamAdded(Stream),
    HaveAllStreams,
    StreamChanged(Stream),
    // StreamsAdded(Vec<Stream>), // Implies HaveAllStreams
    StreamsChanged(Vec<Stream>),
    // TODO need something to replace/add new streams
    // TODO should probably directly implement the GstStreams new world order
    BufferForStream(StreamIndex, gst::Buffer),
    Eos(Option<StreamIndex>),
}

pub type RsDemuxer = RsElement;

pub trait DemuxerImpl: Send + 'static {
    fn start(
        &mut self,
        demuxer: &RsDemuxer,
        upstream_size: Option<u64>,
        random_access: bool,
    ) -> Result<(), ErrorMessage>;
    fn stop(&mut self, demuxer: &RsDemuxer) -> Result<(), ErrorMessage>;

    fn seek(
        &mut self,
        demuxer: &RsDemuxer,
        start: u64,
        stop: Option<u64>,
    ) -> Result<SeekResult, ErrorMessage>;
    fn handle_buffer(
        &mut self,
        demuxer: &RsDemuxer,
        buffer: Option<gst::Buffer>,
    ) -> Result<HandleBufferResult, FlowError>;
    fn end_of_stream(&mut self, demuxer: &RsDemuxer) -> Result<(), ErrorMessage>;

    fn is_seekable(&self, demuxer: &RsDemuxer) -> bool;
    fn get_position(&self, demuxer: &RsDemuxer) -> Option<u64>;
    fn get_duration(&self, demuxer: &RsDemuxer) -> Option<u64>;
}

#[derive(Debug)]
pub struct Stream {
    pub index: StreamIndex,
    pub caps: gst::Caps,
    pub stream_id: String,
}

impl Stream {
    pub fn new(index: StreamIndex, caps: gst::Caps, stream_id: String) -> Stream {
        Stream {
            index: index,
            caps: caps,
            stream_id: stream_id,
        }
    }
}

pub struct DemuxerWrapper {
    cat: gst::DebugCategory,
    demuxer: Mutex<Box<DemuxerImpl>>,
}

impl DemuxerWrapper {
    fn new(demuxer_impl: Box<DemuxerImpl>) -> DemuxerWrapper {
        DemuxerWrapper {
            cat: gst::DebugCategory::new(
                "rsdemux",
                gst::DebugColorFlags::empty(),
                "Rust demuxer base class",
            ),
            demuxer: Mutex::new(demuxer_impl),
        }
    }

    fn start(&self, element: &RsDemuxer, upstream_size: Option<u64>, random_access: bool) -> bool {
        let demuxer_impl = &mut self.demuxer.lock().unwrap();

        gst_debug!(
            self.cat,
            obj: element,
            "Starting with upstream size {:?} and random access {}",
            upstream_size,
            random_access
        );

        match demuxer_impl.start(element, upstream_size, random_access) {
            Ok(..) => {
                gst_trace!(self.cat, obj: element, "Successfully started",);
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: element, "Failed to start: {:?}", msg);
                self.post_message(element, msg);
                false
            }
        }
    }
    fn stop(&self, element: &RsDemuxer) -> bool {
        let demuxer_impl = &mut self.demuxer.lock().unwrap();

        gst_debug!(self.cat, obj: element, "Stopping");

        match demuxer_impl.stop(element) {
            Ok(..) => {
                gst_trace!(self.cat, obj: element, "Successfully stop");
                true
            }
            Err(ref msg) => {
                gst_error!(self.cat, obj: element, "Failed to stop: {:?}", msg);
                self.post_message(element, msg);
                false
            }
        }
    }

    fn is_seekable(&self, element: &RsDemuxer) -> bool {
        let demuxer_impl = &self.demuxer.lock().unwrap();

        let seekable = demuxer_impl.is_seekable(element);
        gst_debug!(self.cat, obj: element, "Seekable {}", seekable);

        seekable
    }


    fn get_position(&self, element: &RsDemuxer) -> Option<u64> {
        let demuxer_impl = &self.demuxer.lock().unwrap();

        demuxer_impl.get_position(element)
    }

    fn get_duration(&self, element: &RsDemuxer) -> Option<u64> {
        let demuxer_impl = &self.demuxer.lock().unwrap();

        demuxer_impl.get_duration(element)
    }

    fn seek(
        &self,
        demuxer: &Demuxer,
        element: &RsDemuxer,
        start: u64,
        stop: u64,
        offset: &mut u64,
    ) -> bool {
        let stop = if stop == u64::MAX { None } else { Some(stop) };

        gst_debug!(self.cat, obj: element, "Seeking to {:?}-{:?}", start, stop);

        let res = {
            let mut demuxer_impl = &mut self.demuxer.lock().unwrap();

            match demuxer_impl.seek(element, start, stop) {
                Ok(res) => res,
                Err(ref msg) => {
                    gst_error!(self.cat, obj: element, "Failed to seek: {:?}", msg);
                    self.post_message(element, msg);
                    return false;
                }
            }
        };

        match res {
            SeekResult::TooEarly => {
                gst_debug!(self.cat, obj: element, "Seeked too early");
                false
            }
            SeekResult::Ok(off) => {
                gst_trace!(self.cat, obj: element, "Seeked successfully");
                *offset = off;
                true
            }
            SeekResult::Eos => {
                gst_debug!(self.cat, obj: element, "Seeked after EOS");
                *offset = u64::MAX;

                demuxer.stream_eos(element, None);

                true
            }
        }
    }

    fn handle_buffer(
        &self,
        demuxer: &Demuxer,
        element: &RsDemuxer,
        buffer: gst::Buffer,
    ) -> gst::FlowReturn {
        let mut res = {
            let mut demuxer_impl = &mut self.demuxer.lock().unwrap();

            gst_trace!(self.cat, obj: element, "Handling buffer {:?}", buffer);

            match demuxer_impl.handle_buffer(element, Some(buffer)) {
                Ok(res) => res,
                Err(flow_error) => {
                    gst_error!(
                        self.cat,
                        obj: element,
                        "Failed handling buffer: {:?}",
                        flow_error
                    );
                    match flow_error {
                        FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                            self.post_message(element, msg)
                        }
                        _ => (),
                    }
                    return flow_error.to_native();
                }
            }
        };

        // Loop until AllEos, NeedMoreData or error when pushing downstream
        loop {
            gst_trace!(self.cat, obj: element, "Handled {:?}", res);

            match res {
                HandleBufferResult::NeedMoreData => {
                    return gst::FlowReturn::Ok;
                }
                HandleBufferResult::StreamAdded(stream) => {
                    demuxer.add_stream(element, stream.index, stream.caps, &stream.stream_id);
                }
                HandleBufferResult::HaveAllStreams => {
                    demuxer.added_all_streams(element);
                }
                HandleBufferResult::StreamChanged(stream) => {
                    demuxer.stream_format_changed(element, stream.index, stream.caps);
                }
                HandleBufferResult::StreamsChanged(streams) => for stream in streams {
                    demuxer.stream_format_changed(element, stream.index, stream.caps);
                },
                HandleBufferResult::BufferForStream(index, buffer) => {
                    let flow_ret = demuxer.stream_push_buffer(element, index, buffer);

                    if flow_ret != gst::FlowReturn::Ok {
                        return flow_ret;
                    }
                }
                HandleBufferResult::Eos(index) => {
                    demuxer.stream_eos(element, index);
                    return gst::FlowReturn::Eos;
                }
                HandleBufferResult::Again => {
                    // nothing, just call again
                }
            };

            gst_trace!(self.cat, obj: element, "Calling again");

            res = {
                let mut demuxer_impl = &mut self.demuxer.lock().unwrap();
                match demuxer_impl.handle_buffer(element, None) {
                    Ok(res) => res,
                    Err(flow_error) => {
                        gst_error!(
                            self.cat,
                            obj: element,
                            "Failed calling again: {:?}",
                            flow_error
                        );
                        match flow_error {
                            FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
                                self.post_message(element, msg)
                            }
                            _ => (),
                        }
                        return flow_error.to_native();
                    }
                }
            }
        }
    }

    fn end_of_stream(&self, element: &RsDemuxer) {
        let mut demuxer_impl = &mut self.demuxer.lock().unwrap();

        gst_debug!(self.cat, obj: element, "End of stream");
        match demuxer_impl.end_of_stream(element) {
            Ok(_) => (),
            Err(ref msg) => {
                gst_error!(self.cat, obj: element, "Failed end of stream: {:?}", msg);
                self.post_message(element, msg);
            }
        }
    }

    fn post_message(&self, element: &RsDemuxer, msg: &ErrorMessage) {
        msg.post(element);
    }
}

pub struct DemuxerInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(&RsDemuxer) -> Box<DemuxerImpl>,
    pub input_caps: gst::Caps,
    pub output_caps: gst::Caps,
}

pub struct Demuxer {
    sinkpad: gst::Pad,
    flow_combiner: Mutex<UniqueFlowCombiner>,
    group_id: Mutex<u32>,
    srcpads: Mutex<BTreeMap<u32, gst::Pad>>,
    wrap: Box<DemuxerWrapper>,
}

#[derive(Default)]
pub struct UniqueFlowCombiner(gst_base::FlowCombiner);

impl UniqueFlowCombiner {
    fn add_pad(&mut self, pad: &gst::Pad) {
        self.0.add_pad(pad);
    }

    fn clear(&mut self) {
        self.0.clear();
    }

    fn update_flow(&mut self, flow_ret: gst::FlowReturn) -> gst::FlowReturn {
        self.0.update_flow(flow_ret)
    }
}

unsafe impl Send for UniqueFlowCombiner {}
unsafe impl Sync for UniqueFlowCombiner {}

impl Demuxer {
    fn get_wrap(&self) -> &DemuxerWrapper {
        &self.wrap
    }

    fn new(demuxer: &RsDemuxer, sinkpad: gst::Pad, demuxer_info: &DemuxerInfo) -> Self {
        Self {
            sinkpad: sinkpad,
            flow_combiner: Mutex::new(Default::default()),
            group_id: Mutex::new(gst::util_group_id_next()),
            srcpads: Mutex::new(BTreeMap::new()),
            wrap: Box::new(DemuxerWrapper::new((demuxer_info.create_instance)(demuxer))),
        }
    }

    fn class_init(
        klass: &mut RsElementClass,
        long_name: &str,
        classification: &str,
        description: &str,
        author: &str,
        input_caps: &gst::Caps,
        output_caps: &gst::Caps,
    ) {
        klass.set_metadata(long_name, classification, description, author);

        let pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            input_caps,
        );
        klass.add_pad_template(pad_template);

        let pad_template = gst::PadTemplate::new(
            "src_%u",
            gst::PadDirection::Src,
            gst::PadPresence::Sometimes,
            output_caps,
        );
        klass.add_pad_template(pad_template);
    }

    fn init(element: &RsElement, demuxer_info: &DemuxerInfo) -> Box<ElementImpl> {
        let templ = element.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, "sink");
        sinkpad.set_activate_function(Demuxer::sink_activate);
        sinkpad.set_activatemode_function(Demuxer::sink_activatemode);
        sinkpad.set_chain_function(Demuxer::sink_chain);
        sinkpad.set_event_function(Demuxer::sink_event);
        element.add_pad(&sinkpad).unwrap();

        let imp = Self::new(element, sinkpad, demuxer_info);
        Box::new(imp)
    }

    fn add_stream(&self, element: &RsElement, index: u32, caps: gst::Caps, stream_id: &str) {
        let mut srcpads = self.srcpads.lock().unwrap();
        assert!(!srcpads.contains_key(&index));

        let templ = element.get_pad_template("src_%u").unwrap();
        let name = format!("src_{}", index);
        let pad = gst::Pad::new_from_template(&templ, Some(name.as_str()));
        pad.set_query_function(Demuxer::src_query);
        pad.set_event_function(Demuxer::src_event);

        pad.set_active(true).unwrap();

        let full_stream_id = pad.create_stream_id(element, stream_id).unwrap();
        pad.push_event(
            gst::Event::new_stream_start(&full_stream_id)
                .group_id(*self.group_id.lock().unwrap())
                .build(),
        );
        pad.push_event(gst::Event::new_caps(&caps).build());

        let mut segment = gst::Segment::default();
        segment.init(gst::Format::Time);
        pad.push_event(gst::Event::new_segment(&segment).build());

        self.flow_combiner.lock().unwrap().add_pad(&pad);
        element.add_pad(&pad).unwrap();

        srcpads.insert(index, pad);
    }

    fn added_all_streams(&self, element: &RsElement) {
        element.no_more_pads();
        *self.group_id.lock().unwrap() = gst::util_group_id_next();
    }

    fn stream_format_changed(&self, _element: &RsElement, index: u32, caps: gst::Caps) {
        let srcpads = self.srcpads.lock().unwrap();

        if let Some(pad) = srcpads.get(&index) {
            pad.push_event(gst::Event::new_caps(&caps).build());
        }
    }

    fn stream_eos(&self, _element: &RsElement, index: Option<u32>) {
        let srcpads = self.srcpads.lock().unwrap();

        let event = gst::Event::new_eos().build();
        match index {
            Some(index) => if let Some(pad) = srcpads.get(&index) {
                pad.push_event(event);
            },
            None => for (_, pad) in srcpads.iter().by_ref() {
                pad.push_event(event.clone());
            },
        };
    }

    fn stream_push_buffer(
        &self,
        _element: &RsElement,
        index: u32,
        buffer: gst::Buffer,
    ) -> gst::FlowReturn {
        let srcpads = self.srcpads.lock().unwrap();

        if let Some(pad) = srcpads.get(&index) {
            self.flow_combiner
                .lock()
                .unwrap()
                .update_flow(pad.push(buffer))
        } else {
            gst::FlowReturn::Error
        }
    }

    fn remove_all_streams(&self, element: &RsElement) {
        self.flow_combiner.lock().unwrap().clear();
        let mut srcpads = self.srcpads.lock().unwrap();
        for (_, pad) in srcpads.iter().by_ref() {
            element.remove_pad(pad).unwrap();
        }
        srcpads.clear();
    }

    fn sink_activate(pad: &gst::Pad, _parent: &Option<gst::Object>) -> bool {
        let mode = {
            let mut query = gst::Query::new_scheduling();
            if !pad.peer_query(query.get_mut().unwrap()) {
                return false;
            }

            // TODO
            //if (gst_query_has_scheduling_mode_with_flags (query, GST_PAD_MODE_PULL, GST_SCHEDULING_FLAG_SEEKABLE)) {
            //  GST_DEBUG_OBJECT (demuxer, "Activating in PULL mode");
            //  mode = GST_PAD_MODE_PULL;
            //} else {
            //GST_DEBUG_OBJECT (demuxer, "Activating in PUSH mode");
            //}
            gst::PadMode::Push
        };

        match pad.activate_mode(mode, true) {
            Ok(_) => true,
            Err(_) => false,
        }
    }

    fn sink_activatemode(
        _pad: &gst::Pad,
        parent: &Option<gst::Object>,
        mode: gst::PadMode,
        active: bool,
    ) -> bool {
        let element = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsElement>()
            .unwrap();
        let demuxer = element.get_impl().downcast_ref::<Demuxer>().unwrap();
        let wrap = demuxer.get_wrap();

        if active {
            let mut query = gst::Query::new_duration(gst::Format::Bytes);
            let upstream_size = if demuxer.sinkpad.peer_query(query.get_mut().unwrap()) {
                use gst::QueryView;

                match query.view() {
                    QueryView::Duration(ref d) => Some(d.get().1 as u64),
                    _ => unreachable!(),
                }
            } else {
                None
            };

            if !wrap.start(&element, upstream_size, mode == gst::PadMode::Pull) {
                return false;
            }

            if mode == gst::PadMode::Pull {
                // TODO
                // demuxer.sinkpad.start_task(...)
            }

            true
        } else {
            if mode == gst::PadMode::Pull {
                let _ = demuxer.sinkpad.stop_task();
            }

            wrap.stop(&element)
        }
    }

    fn sink_chain(
        _pad: &gst::Pad,
        parent: &Option<gst::Object>,
        buffer: gst::Buffer,
    ) -> gst::FlowReturn {
        let element = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsElement>()
            .unwrap();
        let demuxer = element.get_impl().downcast_ref::<Demuxer>().unwrap();
        let wrap = demuxer.get_wrap();
        wrap.handle_buffer(demuxer, &element, buffer)
    }

    fn sink_event(pad: &gst::Pad, parent: &Option<gst::Object>, event: gst::Event) -> bool {
        use gst::EventView;

        let element = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsElement>()
            .unwrap();
        let demuxer = element.get_impl().downcast_ref::<Demuxer>().unwrap();
        let wrap = demuxer.get_wrap();

        match event.view() {
            EventView::Eos(..) => {
                wrap.end_of_stream(&element);
                pad.event_default(parent.as_ref(), event)
            }
            EventView::Segment(..) => pad.event_default(parent.as_ref(), event),
            _ => pad.event_default(parent.as_ref(), event),
        }
    }

    fn src_query(pad: &gst::Pad, parent: &Option<gst::Object>, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        let element = parent
            .as_ref()
            .map(|o| o.clone())
            .unwrap()
            .downcast::<RsElement>()
            .unwrap();
        let demuxer = element.get_impl().downcast_ref::<Demuxer>().unwrap();
        let wrap = demuxer.get_wrap();

        match query.view_mut() {
            QueryView::Position(ref mut q) => {
                let (fmt, _) = q.get();
                if fmt == gst::Format::Time {
                    let position = wrap.get_position(&element);
                    gst_trace!(wrap.cat, obj: &element, "Returning position {:?}", position);

                    match position {
                        None => return false,
                        Some(position) => {
                            q.set(fmt, position as i64);
                            return true;
                        }
                    }
                } else {
                    return false;
                }
            }
            QueryView::Duration(ref mut q) => {
                let (fmt, _) = q.get();
                if fmt == gst::Format::Time {
                    let duration = wrap.get_duration(&element);
                    gst_trace!(wrap.cat, obj: &element, "Returning duration {:?}", duration);

                    match duration {
                        None => return false,
                        Some(duration) => {
                            q.set(fmt, duration as i64);
                            return true;
                        }
                    }
                } else {
                    return false;
                }
            }
            _ => (),
        }

        // FIXME: Have to do it outside the match because otherwise query is already mutably
        // borrowed by the query view.
        pad.query_default(parent.as_ref(), query)
    }

    fn src_event(pad: &gst::Pad, parent: &Option<gst::Object>, event: gst::Event) -> bool {
        use gst::EventView;

        match event.view() {
            EventView::Seek(..) => {
                // TODO: Implement
                false
            }
            _ => pad.event_default(parent.as_ref(), event),
        }
    }
}

impl ElementImpl for Demuxer {
    fn change_state(
        &self,
        element: &RsElement,
        transition: gst::StateChange,
    ) -> gst::StateChangeReturn {
        let mut ret = gst::StateChangeReturn::Success;
        let wrap = self.get_wrap();

        gst_trace!(wrap.cat, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::ReadyToPaused => {
                // TODO
                *self.group_id.lock().unwrap() = gst::util_group_id_next();
            }
            _ => (),
        }

        ret = element.parent_change_state(transition);
        if ret == gst::StateChangeReturn::Failure {
            return ret;
        }

        match transition {
            gst::StateChange::PausedToReady => {
                self.flow_combiner.lock().unwrap().clear();
                let mut srcpads = self.srcpads.lock().unwrap();
                for (_, pad) in srcpads.iter().by_ref() {
                    element.remove_pad(pad).unwrap();
                }
                srcpads.clear();
            }
            _ => (),
        }

        ret
    }
}

pub fn demuxer_register(plugin: &gst::Plugin, demuxer_info: DemuxerInfo) {
    let name = demuxer_info.name.clone();
    let rank = demuxer_info.rank;

    let long_name = demuxer_info.long_name.clone();
    let classification = demuxer_info.classification.clone();
    let description = demuxer_info.description.clone();
    let author = demuxer_info.author.clone();
    let input_caps = demuxer_info.input_caps.clone();
    let output_caps = demuxer_info.output_caps.clone();

    element_register(
        plugin,
        &name,
        rank,
        move |klass| {
            Demuxer::class_init(
                klass,
                &long_name,
                &classification,
                &description,
                &author,
                &input_caps,
                &output_caps,
            )
        },
        move |element| Demuxer::init(element, &demuxer_info),
    );
}
