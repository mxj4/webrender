use app_units::Au;
use batch::{MAX_MATRICES_PER_BATCH, OffsetParams};
use device::{TextureId, TextureFilter};
use euclid::{Rect, Point2D, Point3D, Point4D, Size2D, Matrix4};
use fnv::FnvHasher;
use geometry::ray_intersects_rect;
use internal_types::{AxisDirection, LowLevelFilterOp, CompositionOp, DrawListItemIndex};
use internal_types::{BatchUpdateList, RenderTargetIndex, DrawListId};
use internal_types::{CompositeBatchInfo, CompositeBatchJob};
use internal_types::{RendererFrame, StackingContextInfo, BatchInfo, DrawCall, StackingContextIndex};
use internal_types::{ANGLE_FLOAT_TO_FIXED, MAX_RECT, BatchUpdate, BatchUpdateOp, DrawLayer};
use internal_types::{DrawCommand, ClearInfo, DrawTargetInfo, RenderTargetId, DrawListGroupId};
use layer::Layer;
use node_compiler::NodeCompiler;
use renderer::CompositionOpHelpers;
use resource_cache::ResourceCache;
use resource_list::BuildRequiredResources;
use scene::{SceneStackingContext, ScenePipeline, Scene, SceneItem, SpecificSceneItem};
use scoped_threadpool;
use std::collections::HashMap;
use std::collections::hash_state::DefaultState;
use std::mem;
use texture_cache::TexturePage;
use util;
use webrender_traits::{PipelineId, Epoch, ScrollPolicy, ScrollLayerId, StackingContext};
use webrender_traits::{FilterOp, ImageFormat, MixBlendMode, StackingLevel};

pub struct DrawListGroup {
    pub id: DrawListGroupId,

    // Together, these define the granularity that batches
    // can be created at. When compiling nodes, if either
    // the scroll layer or render target are different from
    // the current batch, it must be broken and a new batch started.
    // This automatically handles the case of CompositeBatch, because
    // for a composite batch to be present, the next draw list must be
    // in a different render target!
    pub scroll_layer_id: ScrollLayerId,
    pub render_target_id: RenderTargetId,

    pub draw_list_ids: Vec<DrawListId>,
}

impl DrawListGroup {
    fn new(id: DrawListGroupId,
           scroll_layer_id: ScrollLayerId,
           render_target_id: RenderTargetId) -> DrawListGroup {
        DrawListGroup {
            id: id,
            scroll_layer_id: scroll_layer_id,
            render_target_id: render_target_id,
            draw_list_ids: Vec::new(),
        }
    }

    fn can_add(&self,
               scroll_layer_id: ScrollLayerId,
               render_target_id: RenderTargetId) -> bool {
        let scroll_ok = scroll_layer_id == self.scroll_layer_id;
        let target_ok = render_target_id == self.render_target_id;
        let size_ok = self.draw_list_ids.len() < MAX_MATRICES_PER_BATCH;
        scroll_ok && target_ok && size_ok
    }

    fn push(&mut self, draw_list_id: DrawListId) {
        self.draw_list_ids.push(draw_list_id);
    }
}

struct FlattenContext<'a> {
    resource_cache: &'a mut ResourceCache,
    scene: &'a Scene,
    pipeline_sizes: &'a mut HashMap<PipelineId, Size2D<f32>>,
    current_draw_list_group: Option<DrawListGroup>,
}

struct FlattenInfo {
    viewport_size: Size2D<f32>,
    current_clip_rect: Rect<f32>,
    default_scroll_layer_id: ScrollLayerId,
    actual_scroll_layer_id: ScrollLayerId,
    offset_from_origin: Point2D<f32>,
    offset_from_current_layer: Point2D<f32>,
    transform: Matrix4,
    perspective: Matrix4,
}

#[derive(Debug)]
pub enum FrameRenderItem {
    Clear(ClearInfo),
    CompositeBatch(CompositeBatchInfo),
    DrawListBatch(DrawListGroupId),
}

pub struct RenderTarget {
    id: RenderTargetId,

    // Child render targets
    children: Vec<RenderTarget>,

    // Outputs
    items: Vec<FrameRenderItem>,

    // Texture id for any child render targets to use
    child_texture_id: Option<TextureId>,

    size: Size2D<u32>,
}

impl RenderTarget {
    fn new(id: RenderTargetId,
           size: Size2D<u32>) -> RenderTarget {
        RenderTarget {
            id: id,
            children: Vec::new(),
            items: Vec::new(),
            child_texture_id: None,
            size: size,
        }
    }

    fn collect_and_sort_visible_batches(&mut self,
                                        resource_cache: &mut ResourceCache,
                                        draw_list_groups: &HashMap<DrawListGroupId, DrawListGroup, DefaultState<FnvHasher>>,
                                        layers: &HashMap<ScrollLayerId, Layer, DefaultState<FnvHasher>>,
                                        stacking_context_info: &Vec<StackingContextInfo>,
                                        device_pixel_ratio: f32) -> DrawLayer {
        let mut commands = vec![];
        for item in &self.items {
            match item {
                &FrameRenderItem::Clear(ref info) => {
                    commands.push(DrawCommand::Clear(info.clone()));
                }
                &FrameRenderItem::CompositeBatch(ref info) => {
                    commands.push(DrawCommand::CompositeBatch(info.clone()));
                }
                &FrameRenderItem::DrawListBatch(draw_list_group_id) => {
                    let draw_list_group = &draw_list_groups[&draw_list_group_id];
                    debug_assert!(draw_list_group.draw_list_ids.len() <= MAX_MATRICES_PER_BATCH);

                    let layer = &layers[&draw_list_group.scroll_layer_id];
                    let mut matrix_palette =
                        vec![Matrix4::identity(); draw_list_group.draw_list_ids.len()];
                    let mut offset_palette =
                        vec![OffsetParams::identity(); draw_list_group.draw_list_ids.len()];

                    // Update batch matrices
                    for (index, draw_list_id) in draw_list_group.draw_list_ids.iter().enumerate() {
                        let draw_list = resource_cache.get_draw_list(*draw_list_id);

                        let StackingContextIndex(stacking_context_id) = draw_list.stacking_context_index.unwrap();
                        let context = &stacking_context_info[stacking_context_id];

                        let transform = layer.world_transform.mul(&context.transform);
                        matrix_palette[index] = transform;

                        offset_palette[index].stacking_context_x0 = context.offset_from_layer.x;
                        offset_palette[index].stacking_context_y0 = context.offset_from_layer.y;
                    }

                    let mut batch_info = BatchInfo::new(matrix_palette, offset_palette);

                    // Collect relevant draws from each node in the tree.
                    for node in &layer.aabb_tree.nodes {
                        if node.is_visible {
                            debug_assert!(node.compiled_node.is_some());
                            let compiled_node = node.compiled_node.as_ref().unwrap();

                            let batch_list = compiled_node.batch_list.iter().find(|batch_list| {
                                batch_list.draw_list_group_id == draw_list_group_id
                            });

                            if let Some(batch_list) = batch_list {
                                let vertex_buffer_id = compiled_node.vertex_buffer_id.unwrap();

                                let scroll_clip_rect = Rect::new(-layer.scroll_offset,
                                                                 layer.viewport_size);

                                for batch in &batch_list.batches {
                                    let mut clip_rects = batch.clip_rects.clone();

                                    // Intersect all local clips for this layer with the viewport
                                    // size. This clips out content outside iframes, scroll layers etc.
                                    for clip_rect in &mut clip_rects {
                                        *clip_rect = match clip_rect.intersection(&scroll_clip_rect) {
                                            Some(clip_rect) => clip_rect,
                                            None => Rect::new(Point2D::zero(), Size2D::zero()),
                                        };
                                    }

                                    batch_info.draw_calls.push(DrawCall {
                                        tile_params: batch.tile_params.clone(),     // TODO(gw): Move this instead?
                                        clip_rects: clip_rects,
                                        vertex_buffer_id: vertex_buffer_id,
                                        color_texture_id: batch.color_texture_id,
                                        mask_texture_id: batch.mask_texture_id,
                                        first_instance: batch.first_instance,
                                        instance_count: batch.instance_count,
                                    });
                                }
                            }
                        }
                    }

                    // Finally, add the batch + draw calls
                    commands.push(DrawCommand::Batch(batch_info));
                }
            }
        }

        let mut child_layers = Vec::new();

        let draw_target_info = if self.children.is_empty() {
            None
        } else {
            let texture_size = 2048;
            let device_pixel_size = texture_size * device_pixel_ratio as u32;

            // TODO(gw): This doesn't handle not having enough space to store
            //           draw all child render targets. However, this will soon
            //           be changing to do the RT allocation in a smarter way
            //           that greatly reduces the # of active RT allocations.
            //           When that happens, ensure it handles this case!
            if let Some(child_texture_id) = self.child_texture_id.take() {
                resource_cache.free_render_target(child_texture_id);
            }

            self.child_texture_id = Some(resource_cache.allocate_render_target(device_pixel_size,
                                                                               device_pixel_size,
                                                                               ImageFormat::RGBA8));

            // TODO(gw): Move this texture page allocator based on the suggested changes above.
            let mut page = TexturePage::new(self.child_texture_id.unwrap(), texture_size);

            for child in &mut self.children {
                let mut child_layer = child.collect_and_sort_visible_batches(resource_cache,
                                                                             draw_list_groups,
                                                                             layers,
                                                                             stacking_context_info,
                                                                             device_pixel_ratio);

                child_layer.layer_origin = page.allocate(&child_layer.layer_size,
                                                         TextureFilter::Linear).unwrap();
                child_layers.push(child_layer);
            }

            Some(DrawTargetInfo {
                size: Size2D::new(texture_size, texture_size),
                texture_id: self.child_texture_id.unwrap(),
            })
        };

        DrawLayer::new(draw_target_info,
                       child_layers,
                       commands,
                       self.size)
    }

    fn reset(&mut self,
             pending_updates: &mut BatchUpdateList,
             resource_cache: &mut ResourceCache) {
        if let Some(child_texture_id) = self.child_texture_id.take() {
            resource_cache.free_render_target(child_texture_id);
        }

        for mut child in &mut self.children.drain(..) {
            child.reset(pending_updates,
                        resource_cache);
        }

        self.items.clear();
    }

    fn push_clear(&mut self, clear_info: ClearInfo) {
        self.items.push(FrameRenderItem::Clear(clear_info));
    }

    fn push_composite(&mut self,
                      op: CompositionOp,
                      target: Rect<i32>,
                      render_target_index: RenderTargetIndex) {
        // TODO(gw): Relax the restriction on batch breaks for FB reads
        //           once the proper render target allocation code is done!
        let need_new_batch = op.needs_framebuffer() || match self.items.last() {
            Some(&FrameRenderItem::CompositeBatch(ref info)) => {
                info.operation != op
            }
            Some(&FrameRenderItem::Clear(..)) |
            Some(&FrameRenderItem::DrawListBatch(..)) |
            None => {
                true
            }
        };

        if need_new_batch {
            self.items.push(FrameRenderItem::CompositeBatch(CompositeBatchInfo {
                operation: op,
                jobs: Vec::new(),
            }));
        }

        // TODO(gw): This seems a little messy - restructure how current batch works!
        match self.items.last_mut().unwrap() {
            &mut FrameRenderItem::CompositeBatch(ref mut batch) => {
                let job = CompositeBatchJob {
                    rect: target,
                    render_target_index: render_target_index
                };
                batch.jobs.push(job);
            }
            _ => {
                unreachable!();
            }
        }
    }

    fn push_draw_list_group(&mut self, draw_list_group_id: DrawListGroupId) {
        self.items.push(FrameRenderItem::DrawListBatch(draw_list_group_id));
    }
}

pub struct Frame {
    pub layers: HashMap<ScrollLayerId, Layer, DefaultState<FnvHasher>>,
    pub pipeline_epoch_map: HashMap<PipelineId, Epoch, DefaultState<FnvHasher>>,
    pub pending_updates: BatchUpdateList,
    pub root: Option<RenderTarget>,
    pub stacking_context_info: Vec<StackingContextInfo>,
    next_render_target_id: RenderTargetId,
    next_draw_list_group_id: DrawListGroupId,
    draw_list_groups: HashMap<DrawListGroupId, DrawListGroup, DefaultState<FnvHasher>>,
    root_scroll_layer_id: Option<ScrollLayerId>,
}

enum SceneItemKind<'a> {
    StackingContext(&'a SceneStackingContext),
    Pipeline(&'a ScenePipeline)
}

#[derive(Clone)]
struct SceneItemWithZOrder {
    item: SceneItem,
    z_index: i32,
}

impl<'a> SceneItemKind<'a> {
    fn collect_scene_items(&self, scene: &Scene) -> Vec<SceneItem> {
        let mut background_and_borders = Vec::new();
        let mut positioned_content = Vec::new();
        let mut block_background_and_borders = Vec::new();
        let mut floats = Vec::new();
        let mut content = Vec::new();
        let mut outlines = Vec::new();

        let stacking_context = match *self {
            SceneItemKind::StackingContext(stacking_context) => {
                &stacking_context.stacking_context
            }
            SceneItemKind::Pipeline(pipeline) => {
                if let Some(background_draw_list) = pipeline.background_draw_list {
                    background_and_borders.push(SceneItem {
                        stacking_level: StackingLevel::BackgroundAndBorders,
                        specific: SpecificSceneItem::DrawList(background_draw_list),
                    });
                }

                &scene.stacking_context_map
                      .get(&pipeline.root_stacking_context_id)
                      .unwrap()
                      .stacking_context
            }
        };

        for display_list_id in &stacking_context.display_lists {
            let display_list = &scene.display_list_map[display_list_id];
            for item in &display_list.items {
                match item.stacking_level {
                    StackingLevel::BackgroundAndBorders => {
                        background_and_borders.push(item.clone());
                    }
                    StackingLevel::BlockBackgroundAndBorders => {
                        block_background_and_borders.push(item.clone());
                    }
                    StackingLevel::PositionedContent => {
                        let z_index = match item.specific {
                            SpecificSceneItem::StackingContext(id) => {
                                scene.stacking_context_map
                                     .get(&id)
                                     .unwrap()
                                     .stacking_context
                                     .z_index
                            }
                            SpecificSceneItem::DrawList(..) |
                            SpecificSceneItem::Iframe(..) => {
                                // TODO(gw): Probably wrong for an iframe?
                                0
                            }
                        };

                        positioned_content.push(SceneItemWithZOrder {
                            item: item.clone(),
                            z_index: z_index,
                        });
                    }
                    StackingLevel::Floats => {
                        floats.push(item.clone());
                    }
                    StackingLevel::Content => {
                        content.push(item.clone());
                    }
                    StackingLevel::Outlines => {
                        outlines.push(item.clone());
                    }
                }
            }
        }

        positioned_content.sort_by(|a, b| {
            a.z_index.cmp(&b.z_index)
        });

        let mut result = Vec::new();
        result.extend(background_and_borders);
        result.extend(positioned_content.iter().filter_map(|item| {
            if item.z_index < 0 {
                Some(item.item.clone())
            } else {
                None
            }
        }));
        result.extend(block_background_and_borders);
        result.extend(floats);
        result.extend(content);
        result.extend(positioned_content.iter().filter_map(|item| {
            if item.z_index < 0 {
                None
            } else {
                Some(item.item.clone())
            }
        }));
        result.extend(outlines);
        result
    }
}

trait StackingContextHelpers {
    fn needs_composition_operation_for_mix_blend_mode(&self) -> bool;
    fn composition_operations(&self) -> Vec<CompositionOp>;
}

impl StackingContextHelpers for StackingContext {
    fn needs_composition_operation_for_mix_blend_mode(&self) -> bool {
        match self.mix_blend_mode {
            MixBlendMode::Normal => false,
            MixBlendMode::Multiply |
            MixBlendMode::Screen |
            MixBlendMode::Overlay |
            MixBlendMode::Darken |
            MixBlendMode::Lighten |
            MixBlendMode::ColorDodge |
            MixBlendMode::ColorBurn |
            MixBlendMode::HardLight |
            MixBlendMode::SoftLight |
            MixBlendMode::Difference |
            MixBlendMode::Exclusion |
            MixBlendMode::Hue |
            MixBlendMode::Saturation |
            MixBlendMode::Color |
            MixBlendMode::Luminosity => true,
        }
    }

    fn composition_operations(&self) -> Vec<CompositionOp> {
        let mut composition_operations = vec![];
        if self.needs_composition_operation_for_mix_blend_mode() {
            composition_operations.push(CompositionOp::MixBlend(self.mix_blend_mode));
        }
        for filter in self.filters.iter() {
            match *filter {
                FilterOp::Blur(radius) => {
                    composition_operations.push(CompositionOp::Filter(LowLevelFilterOp::Blur(
                        radius,
                        AxisDirection::Horizontal)));
                    composition_operations.push(CompositionOp::Filter(LowLevelFilterOp::Blur(
                        radius,
                        AxisDirection::Vertical)));
                }
                FilterOp::Brightness(amount) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::Brightness(Au::from_f32_px(amount))));
                }
                FilterOp::Contrast(amount) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::Contrast(Au::from_f32_px(amount))));
                }
                FilterOp::Grayscale(amount) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::Grayscale(Au::from_f32_px(amount))));
                }
                FilterOp::HueRotate(angle) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::HueRotate(f32::round(
                                    angle * ANGLE_FLOAT_TO_FIXED) as i32)));
                }
                FilterOp::Invert(amount) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::Invert(Au::from_f32_px(amount))));
                }
                FilterOp::Opacity(amount) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::Opacity(Au::from_f32_px(amount))));
                }
                FilterOp::Saturate(amount) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::Saturate(Au::from_f32_px(amount))));
                }
                FilterOp::Sepia(amount) => {
                    composition_operations.push(CompositionOp::Filter(
                            LowLevelFilterOp::Sepia(Au::from_f32_px(amount))));
                }
            }
        }

        composition_operations
    }
}

impl Frame {
    pub fn new() -> Frame {
        Frame {
            pipeline_epoch_map: HashMap::with_hash_state(Default::default()),
            pending_updates: BatchUpdateList::new(),
            root: None,
            layers: HashMap::with_hash_state(Default::default()),
            stacking_context_info: Vec::new(),
            next_render_target_id: RenderTargetId(0),
            next_draw_list_group_id: DrawListGroupId(0),
            draw_list_groups: HashMap::with_hash_state(Default::default()),
            root_scroll_layer_id: None,
        }
    }

    pub fn reset(&mut self, resource_cache: &mut ResourceCache)
                 -> HashMap<ScrollLayerId, Point2D<f32>, DefaultState<FnvHasher>> {
        self.draw_list_groups.clear();
        self.pipeline_epoch_map.clear();
        self.stacking_context_info.clear();

        if let Some(mut root) = self.root.take() {
            root.reset(&mut self.pending_updates, resource_cache);
        }

        // Free any render targets from last frame.
        // TODO: This should really re-use existing targets here...
        let mut old_layer_offsets = HashMap::with_hash_state(Default::default());
        for (layer_id, mut old_layer) in &mut self.layers.drain() {
            old_layer.reset(&mut self.pending_updates);
            old_layer_offsets.insert(layer_id, old_layer.scroll_offset);
        }

        old_layer_offsets
    }

    fn next_render_target_id(&mut self) -> RenderTargetId {
        let RenderTargetId(render_target_id) = self.next_render_target_id;
        self.next_render_target_id = RenderTargetId(render_target_id + 1);
        RenderTargetId(render_target_id)
    }

    fn next_draw_list_group_id(&mut self) -> DrawListGroupId {
        let DrawListGroupId(draw_list_group_id) = self.next_draw_list_group_id;
        self.next_draw_list_group_id = DrawListGroupId(draw_list_group_id + 1);
        DrawListGroupId(draw_list_group_id)
    }

    pub fn pending_updates(&mut self) -> BatchUpdateList {
        mem::replace(&mut self.pending_updates, BatchUpdateList::new())
    }

    pub fn get_scroll_layer(&self,
                            cursor: &Point2D<f32>,
                            scroll_layer_id: ScrollLayerId,
                            parent_transform: &Matrix4) -> Option<ScrollLayerId> {
        self.layers.get(&scroll_layer_id).and_then(|layer| {
            let transform = parent_transform.mul(&layer.local_transform);

            for child_layer_id in &layer.children {
                if let Some(layer_id) = self.get_scroll_layer(cursor,
                                                              *child_layer_id,
                                                              &transform) {
                    return Some(layer_id);
                }
            }

            match scroll_layer_id {
                ScrollLayerId::Fixed => {
                    None
                }
                ScrollLayerId::Normal(..) => {
                    let inv = transform.invert();
                    let z0 = -10000.0;
                    let z1 =  10000.0;

                    let p0 = inv.transform_point4d(&Point4D::new(cursor.x, cursor.y, z0, 1.0));
                    let p0 = Point3D::new(p0.x / p0.w,
                                          p0.y / p0.w,
                                          p0.z / p0.w);
                    let p1 = inv.transform_point4d(&Point4D::new(cursor.x, cursor.y, z1, 1.0));
                    let p1 = Point3D::new(p1.x / p1.w,
                                          p1.y / p1.w,
                                          p1.z / p1.w);

                    let layer_rect = Rect::new(layer.world_origin, layer.viewport_size);

                    if ray_intersects_rect(p0, p1, layer_rect) {
                        Some(scroll_layer_id)
                    } else {
                        None
                    }
                }
            }
        })
    }

    pub fn scroll(&mut self,
                  delta: Point2D<f32>,
                  cursor: Point2D<f32>) {
        if let Some(root_scroll_layer_id) = self.root_scroll_layer_id {
            let scroll_layer_id = self.get_scroll_layer(&cursor,
                                                        root_scroll_layer_id,
                                                        &Matrix4::identity());

            if let Some(scroll_layer_id) = scroll_layer_id {
                let layer = self.layers.get_mut(&scroll_layer_id).unwrap();

                if layer.layer_size.width > layer.viewport_size.width {
                    layer.scroll_offset.x = layer.scroll_offset.x + delta.x;
                    layer.scroll_offset.x = layer.scroll_offset.x.min(0.0);
                    layer.scroll_offset.x = layer.scroll_offset.x.max(-layer.layer_size.width + layer.viewport_size.width);
                }

                if layer.layer_size.height > layer.viewport_size.height {
                    layer.scroll_offset.y = layer.scroll_offset.y + delta.y;
                    layer.scroll_offset.y = layer.scroll_offset.y.min(0.0);
                    layer.scroll_offset.y = layer.scroll_offset.y.max(-layer.layer_size.height + layer.viewport_size.height);
                }

                layer.scroll_offset.x = layer.scroll_offset.x.round();
                layer.scroll_offset.y = layer.scroll_offset.y.round();
            }
        }
    }

    pub fn create(&mut self,
                  scene: &Scene,
                  resource_cache: &mut ResourceCache,
                  pipeline_sizes: &mut HashMap<PipelineId, Size2D<f32>>,
                  viewport_size: Size2D<u32>) {
        if let Some(root_pipeline_id) = scene.root_pipeline_id {
            if let Some(root_pipeline) = scene.pipeline_map.get(&root_pipeline_id) {
                let old_layer_offsets = self.reset(resource_cache);

                let root_stacking_context = scene.stacking_context_map
                                                 .get(&root_pipeline.root_stacking_context_id)
                                                 .unwrap();

                let root_scroll_layer_id = root_stacking_context.stacking_context
                                                                .scroll_layer_id
                                                                .expect("root layer must be a scroll layer!");
                self.root_scroll_layer_id = Some(root_scroll_layer_id);

                let root_target_id = self.next_render_target_id();

                let mut root_target = RenderTarget::new(root_target_id,
                                                        viewport_size);

                // Insert global position: fixed elements layer
                debug_assert!(self.layers.is_empty());
                self.layers.insert(ScrollLayerId::fixed_layer(),
                                   Layer::new(root_stacking_context.stacking_context.overflow.origin,
                                              root_stacking_context.stacking_context.overflow.size,
                                              Size2D::new(viewport_size.width as f32,
                                                          viewport_size.height as f32),
                                              Matrix4::identity()));

                // Work around borrow check on resource cache
                {
                    let mut context = FlattenContext {
                        resource_cache: resource_cache,
                        scene: scene,
                        pipeline_sizes: pipeline_sizes,
                        current_draw_list_group: None,
                    };

                    let parent_info = FlattenInfo {
                        viewport_size: Size2D::new(viewport_size.width as f32, viewport_size.height as f32),
                        offset_from_origin: Point2D::zero(),
                        offset_from_current_layer: Point2D::zero(),
                        default_scroll_layer_id: root_scroll_layer_id,
                        actual_scroll_layer_id: root_scroll_layer_id,
                        current_clip_rect: MAX_RECT,
                        transform: Matrix4::identity(),
                        perspective: Matrix4::identity(),
                    };

                    let root_pipeline = SceneItemKind::Pipeline(root_pipeline);
                    self.flatten(root_pipeline,
                                 &parent_info,
                                 &mut context,
                                 &mut root_target,
                                 0);
                    self.root = Some(root_target);

                    if let Some(last_draw_list_group) = context.current_draw_list_group.take() {
                        self.draw_list_groups.insert(last_draw_list_group.id,
                                                     last_draw_list_group);
                    }
                }

                // TODO(gw): These are all independent - can be run through thread pool if it shows up in the profile!
                for (scroll_layer_id, layer) in &mut self.layers {
                    let scroll_offset = match old_layer_offsets.get(&scroll_layer_id) {
                        Some(old_offset) => *old_offset,
                        None => Point2D::zero(),
                    };

                    layer.finalize(scroll_offset);
                }
            }
        }
    }

    fn add_items_to_target(&mut self,
                           scene_items: &Vec<SceneItem>,
                           info: &FlattenInfo,
                           target: &mut RenderTarget,
                           context: &mut FlattenContext,
                           _level: i32) {
        let stacking_context_index = StackingContextIndex(self.stacking_context_info.len());
        self.stacking_context_info.push(StackingContextInfo {
            offset_from_layer: info.offset_from_current_layer,
            local_clip_rect: info.current_clip_rect,
            transform: info.transform,
            perspective: info.perspective,
        });

        for item in scene_items {
            match item.specific {
                SpecificSceneItem::DrawList(draw_list_id) => {
                    let draw_list = context.resource_cache.get_draw_list_mut(draw_list_id);

                    // Store draw context
                    draw_list.stacking_context_index = Some(stacking_context_index);

                    let needs_new_draw_group = match context.current_draw_list_group {
                        Some(ref draw_list_group) => {
                            !draw_list_group.can_add(info.actual_scroll_layer_id,
                                                     target.id)
                        }
                        None => {
                            true
                        }
                    };

                    if needs_new_draw_group {
                        if let Some(draw_list_group) = context.current_draw_list_group.take() {
                            self.draw_list_groups.insert(draw_list_group.id,
                                                         draw_list_group);
                        }

                        let draw_list_group_id = self.next_draw_list_group_id();

                        let new_draw_list_group = DrawListGroup::new(draw_list_group_id,
                                                                     info.actual_scroll_layer_id,
                                                                     target.id);

                        target.push_draw_list_group(draw_list_group_id);

                        context.current_draw_list_group = Some(new_draw_list_group);
                    }

                    context.current_draw_list_group.as_mut().unwrap().push(draw_list_id);

                    let draw_list_group_id = context.current_draw_list_group.as_ref().unwrap().id;
                    let layer = self.layers.get_mut(&info.actual_scroll_layer_id).unwrap();
                    for (item_index, item) in draw_list.items.iter().enumerate() {
                        let item_index = DrawListItemIndex(item_index as u32);
                        let rect = item.rect
                                       .translate(&info.offset_from_current_layer);
                        layer.insert(rect,
                                     draw_list_group_id,
                                     draw_list_id,
                                     item_index);
                    }
                }
                SpecificSceneItem::StackingContext(id) => {
                    let stacking_context = context.scene
                                                  .stacking_context_map
                                                  .get(&id)
                                                  .unwrap();

                    let child = SceneItemKind::StackingContext(stacking_context);
                    self.flatten(child,
                                 info,
                                 context,
                                 target,
                                 _level+1);
                }
                SpecificSceneItem::Iframe(ref iframe_info) => {
                    let pipeline = context.scene
                                          .pipeline_map
                                          .get(&iframe_info.id);

                    context.pipeline_sizes.insert(iframe_info.id,
                                                  iframe_info.bounds.size);

                    if let Some(pipeline) = pipeline {
                        let iframe = SceneItemKind::Pipeline(pipeline);

                        let iframe_info = FlattenInfo {
                            viewport_size: iframe_info.bounds.size,
                            offset_from_origin: info.offset_from_origin + iframe_info.bounds.origin,
                            offset_from_current_layer: info.offset_from_current_layer + iframe_info.bounds.origin,
                            default_scroll_layer_id: info.default_scroll_layer_id,
                            actual_scroll_layer_id: info.actual_scroll_layer_id,
                            current_clip_rect: MAX_RECT,
                            transform: info.transform,
                            perspective: info.perspective,
                        };

                        self.flatten(iframe,
                                     &iframe_info,
                                     context,
                                     target,
                                     _level+1);
                    }
                }
            }
        }
    }

    fn flatten(&mut self,
               scene_item: SceneItemKind,
               parent_info: &FlattenInfo,
               context: &mut FlattenContext,
               target: &mut RenderTarget,
               level: i32) {
        let _pf = util::ProfileScope::new("  flatten");

        let stacking_context = match scene_item {
            SceneItemKind::StackingContext(stacking_context) => {
                &stacking_context.stacking_context
            }
            SceneItemKind::Pipeline(pipeline) => {
                self.pipeline_epoch_map.insert(pipeline.pipeline_id, pipeline.epoch);

                &context.scene.stacking_context_map
                        .get(&pipeline.root_stacking_context_id)
                        .unwrap()
                        .stacking_context
            }
        };

        let local_clip_rect = parent_info.current_clip_rect
                                         .translate(&-stacking_context.bounds.origin)
                                         .intersection(&stacking_context.overflow);

        if let Some(local_clip_rect) = local_clip_rect {
            let scene_items = scene_item.collect_scene_items(&context.scene);
            if !scene_items.is_empty() {

                // Build world space transform
                let origin = parent_info.offset_from_current_layer + stacking_context.bounds.origin;
                let local_transform = Matrix4::identity().translate(origin.x, origin.y, 0.0)
                                                         .mul(&stacking_context.transform)
                                                         .translate(-origin.x, -origin.y, 0.0);

                let transform = parent_info.perspective.mul(&parent_info.transform)
                                                       .mul(&local_transform);

                // Build world space perspective transform
                let perspective = Matrix4::identity().translate(origin.x, origin.y, 0.0)
                                                     .mul(&stacking_context.perspective)
                                                     .translate(-origin.x, -origin.y, 0.0);

                let mut info = FlattenInfo {
                    viewport_size: parent_info.viewport_size,
                    offset_from_origin: parent_info.offset_from_origin + stacking_context.bounds.origin,
                    offset_from_current_layer: parent_info.offset_from_current_layer + stacking_context.bounds.origin,
                    default_scroll_layer_id: parent_info.default_scroll_layer_id,
                    actual_scroll_layer_id: parent_info.default_scroll_layer_id,
                    current_clip_rect: local_clip_rect,
                    transform: transform,
                    perspective: perspective,
                };

                match (stacking_context.scroll_policy, stacking_context.scroll_layer_id) {
                    (ScrollPolicy::Fixed, _scroll_layer_id) => {
                        debug_assert!(_scroll_layer_id.is_none());
                        info.actual_scroll_layer_id = ScrollLayerId::fixed_layer();
                    }
                    (ScrollPolicy::Scrollable, Some(scroll_layer_id)) => {
                        debug_assert!(!self.layers.contains_key(&scroll_layer_id));
                        let layer = Layer::new(parent_info.offset_from_origin,
                                               stacking_context.overflow.size,
                                               parent_info.viewport_size,
                                               transform);
                        if parent_info.actual_scroll_layer_id != scroll_layer_id {
                            self.layers.get_mut(&parent_info.actual_scroll_layer_id).unwrap().add_child(scroll_layer_id);
                        }
                        self.layers.insert(scroll_layer_id, layer);
                        info.default_scroll_layer_id = scroll_layer_id;
                        info.actual_scroll_layer_id = scroll_layer_id;
                        info.offset_from_current_layer = Point2D::zero();
                        info.transform = Matrix4::identity();
                        info.perspective = Matrix4::identity();
                    }
                    (ScrollPolicy::Scrollable, None) => {
                        // Nothing to do - use defaults as set above.
                    }
                }

                // When establishing a new 3D context, clear Z. This is only needed if there
                // are child stacking contexts, otherwise it is a redundant clear.
                if stacking_context.establishes_3d_context &&
                   stacking_context.has_stacking_contexts {
                    target.push_clear(ClearInfo {
                        clear_color: false,
                        clear_z: true,
                        clear_stencil: true,
                    });
                }

                // TODO: Account for scroll offset with transforms!
                let composition_operations = stacking_context.composition_operations();
                if composition_operations.is_empty() {
                    self.add_items_to_target(&scene_items,
                                             &info,
                                             target,
                                             context,
                                             level);
                } else {
                    let target_size = Size2D::new(local_clip_rect.size.width as i32,
                                                  local_clip_rect.size.height as i32);
                    let target_origin = Point2D::new(info.offset_from_origin.x as i32,
                                                     info.offset_from_origin.y as i32);
                    let unfiltered_target_rect = Rect::new(target_origin, target_size);
                    let mut target_rect = unfiltered_target_rect;
                    for composition_operation in &composition_operations {
                        target_rect = composition_operation.target_rect(&target_rect);
                    }

                    let render_target_index = RenderTargetIndex(target.children.len() as u32);

                    let render_target_size = Size2D::new(target_rect.size.width as u32,
                                                         target_rect.size.height as u32);
                    let render_target_id = self.next_render_target_id();
                    let mut new_target = RenderTarget::new(render_target_id,
                                                           render_target_size);

                    // TODO(gw): Handle transforms + composition ops...
                    for composition_operation in composition_operations {
                        target.push_composite(composition_operation,
                                              target_rect,
                                              render_target_index);
                    }

                    info.offset_from_current_layer = Point2D::zero();

                    self.add_items_to_target(&scene_items,
                                             &info,
                                             &mut new_target,
                                             context,
                                             level);

                    target.children.push(new_target);
                }
            }
        }
    }

    pub fn build(&mut self,
                 resource_cache: &mut ResourceCache,
                 thread_pool: &mut scoped_threadpool::Pool,
                 device_pixel_ratio: f32)
                 -> RendererFrame {
        // Traverse layer trees to calculate visible nodes
        for (_, layer) in &mut self.layers {
            layer.cull();
        }

        // Build resource list for newly visible nodes
        self.update_resource_lists(resource_cache, thread_pool);

        // Update texture cache and build list of raster jobs.
        self.update_texture_cache_and_build_raster_jobs(resource_cache);

        // Rasterize needed glyphs on worker threads
        self.raster_glyphs(thread_pool,
                           resource_cache);

        // Compile nodes that have become visible
        self.compile_visible_nodes(thread_pool,
                                   resource_cache,
                                   device_pixel_ratio);

        // Update the batch cache from newly compiled nodes
        self.update_batch_cache();

        // Update the layer transform matrices
        self.update_layer_transforms();

        // Collect the visible batches into a frame
        let frame = self.collect_and_sort_visible_batches(resource_cache, device_pixel_ratio);

        frame
    }

    fn update_layer_transform(&mut self,
                              layer_id: ScrollLayerId,
                              parent_transform: &Matrix4) {
        // TODO(gw): This is an ugly borrow check workaround to clone these.
        //           Restructure this to avoid the clones!
        let (layer_transform, layer_children) = {
            match self.layers.get_mut(&layer_id) {
                Some(layer) => {
                    layer.world_transform = parent_transform.mul(&layer.local_transform)
                                                            .translate(layer.world_origin.x, layer.world_origin.y, 0.0)
                                                            .translate(layer.scroll_offset.x, layer.scroll_offset.y, 0.0);
                    (layer.world_transform, layer.children.clone())
                }
                None => {
                    return;
                }
            }
        };

        for child_layer_id in layer_children {
            self.update_layer_transform(child_layer_id, &layer_transform);
        }
    }

    fn update_layer_transforms(&mut self) {
        if let Some(root_scroll_layer_id) = self.root_scroll_layer_id {
            self.update_layer_transform(root_scroll_layer_id, &Matrix4::identity());
        }
    }

    pub fn update_resource_lists(&mut self,
                                 resource_cache: &ResourceCache,
                                 thread_pool: &mut scoped_threadpool::Pool) {
        let _pf = util::ProfileScope::new("  update_resource_lists");

        for (_, layer) in &mut self.layers {
            let nodes = &mut layer.aabb_tree.nodes;

            thread_pool.scoped(|scope| {
                for node in nodes {
                    if node.is_visible && node.compiled_node.is_none() {
                        scope.execute(move || {
                            node.build_resource_list(resource_cache);
                        });
                    }
                }
            });
        }
    }

    pub fn update_texture_cache_and_build_raster_jobs(&mut self,
                                                      resource_cache: &mut ResourceCache) {
        let _pf = util::ProfileScope::new("  update_texture_cache_and_build_raster_jobs");

        for (_, layer) in &self.layers {
            for node in &layer.aabb_tree.nodes {
                if node.is_visible {
                    let resource_list = node.resource_list.as_ref().unwrap();
                    resource_cache.add_resource_list(resource_list);
                }
            }
        }
    }

    pub fn raster_glyphs(&mut self,
                     thread_pool: &mut scoped_threadpool::Pool,
                     resource_cache: &mut ResourceCache) {
        let _pf = util::ProfileScope::new("  raster_glyphs");
        resource_cache.raster_pending_glyphs(thread_pool);
    }

    pub fn compile_visible_nodes(&mut self,
                                 thread_pool: &mut scoped_threadpool::Pool,
                                 resource_cache: &ResourceCache,
                                 device_pixel_ratio: f32) {
        let _pf = util::ProfileScope::new("  compile_visible_nodes");

        let layers = &mut self.layers;
        let stacking_context_info = &self.stacking_context_info;
        let draw_list_groups = &self.draw_list_groups;

        thread_pool.scoped(|scope| {
            for (_, layer) in layers {
                let nodes = &mut layer.aabb_tree.nodes;
                for node in nodes {
                    if node.is_visible && node.compiled_node.is_none() {
                        scope.execute(move || {
                            node.compile(resource_cache,
                                         device_pixel_ratio,
                                         stacking_context_info,
                                         draw_list_groups);
                        });
                    }
                }
            }
        });
    }

    pub fn update_batch_cache(&mut self) {
        let _pf = util::ProfileScope::new("  update_batch_cache");

        // Allocate and update VAOs
        for (_, layer) in &mut self.layers {
            for node in &mut layer.aabb_tree.nodes {
                if node.is_visible {
                    let compiled_node = node.compiled_node.as_mut().unwrap();
                    if let Some(vertex_buffer) = compiled_node.vertex_buffer.take() {
                        debug_assert!(compiled_node.vertex_buffer_id.is_none());

                        self.pending_updates.push(BatchUpdate {
                            id: vertex_buffer.id,
                            op: BatchUpdateOp::Create(vertex_buffer.vertices),
                        });

                        compiled_node.vertex_buffer_id = Some(vertex_buffer.id);
                    }
                }
            }
        }
    }

    pub fn collect_and_sort_visible_batches(&mut self,
                                            resource_cache: &mut ResourceCache,
                                            device_pixel_ratio: f32)
                                            -> RendererFrame {
        let root_layer = match self.root {
            Some(ref mut root) => {
                 root.collect_and_sort_visible_batches(resource_cache,
                                                       &self.draw_list_groups,
                                                       &self.layers,
                                                       &self.stacking_context_info,
                                                       device_pixel_ratio)
            }
            None => {
                DrawLayer::new(None,
                               Vec::new(),
                               Vec::new(),
                               Size2D::zero())
            }
        };

        RendererFrame::new(self.pipeline_epoch_map.clone(), root_layer)
    }
}
