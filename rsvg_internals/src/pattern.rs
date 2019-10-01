use cairo;
use markup5ever::local_name;
use std::cell::RefCell;
use std::f64;

use crate::allowed_url::Fragment;
use crate::aspect_ratio::*;
use crate::bbox::*;
use crate::coord_units::CoordUnits;
use crate::drawing_ctx::{DrawingCtx, NodeStack};
use crate::error::{AttributeResultExt, PaintServerError, RenderingError};
use crate::float_eq_cairo::ApproxEqCairo;
use crate::length::*;
use crate::node::*;
use crate::paint_server::{PaintSource, ResolvedPaintSource};
use crate::parsers::ParseValue;
use crate::properties::ComputedValues;
use crate::property_bag::PropertyBag;
use crate::rect::RectangleExt;
use crate::unit_interval::UnitInterval;
use crate::viewbox::*;

coord_units!(PatternUnits, CoordUnits::ObjectBoundingBox);
coord_units!(PatternContentUnits, CoordUnits::UserSpaceOnUse);

#[derive(Clone, Default)]
struct Common {
    units: Option<PatternUnits>,
    content_units: Option<PatternContentUnits>,
    // This Option<Option<ViewBox>> is a bit strange.  We want a field
    // with value None to mean, "this field isn't resolved yet".  However,
    // the vbox can very well be *not* specified in the SVG file.
    // In that case, the fully resolved pattern will have a .vbox=Some(None) value.
    vbox: Option<Option<ViewBox>>,
    preserve_aspect_ratio: Option<AspectRatio>,
    affine: Option<cairo::Matrix>,
    x: Option<LengthHorizontal>,
    y: Option<LengthVertical>,
    width: Option<LengthHorizontal>,
    height: Option<LengthVertical>,

}

#[derive(Clone, Default)]
pub struct NodePattern {
    common: Common,

    // Point back to our corresponding node, or to the fallback node which has children.
    // If the value is None, it means we are fully resolved and didn't find any children
    // among the fallbacks.
    node: RefCell<Option<RsvgNode>>,

    fallback: Option<Fragment>,
}

impl NodeTrait for NodePattern {
    fn set_atts(&mut self, _: Option<&RsvgNode>, pbag: &PropertyBag<'_>) -> NodeResult {
        for (attr, value) in pbag.iter() {
            match attr {
                local_name!("patternUnits") => self.common.units = Some(attr.parse(value)?),
                local_name!("patternContentUnits") => self.common.content_units = Some(attr.parse(value)?),
                local_name!("viewBox") => self.common.vbox = Some(Some(attr.parse(value)?)),
                local_name!("preserveAspectRatio") => {
                    self.common.preserve_aspect_ratio = Some(attr.parse(value)?)
                }
                local_name!("patternTransform") => self.common.affine = Some(attr.parse(value)?),
                local_name!("xlink:href") => {
                    self.fallback = Some(Fragment::parse(value).attribute(attr)?);
                }
                local_name!("x") => self.common.x = Some(attr.parse(value)?),
                local_name!("y") => self.common.y = Some(attr.parse(value)?),
                local_name!("width") => {
                    self.common.width =
                        Some(attr.parse_and_validate(value, LengthHorizontal::check_nonnegative)?)
                }
                local_name!("height") => {
                    self.common.height =
                        Some(attr.parse_and_validate(value, LengthVertical::check_nonnegative)?)
                }
                _ => (),
            }
        }

        Ok(())
    }

    fn overflow_hidden(&self) -> bool {
        true
    }
}

impl PaintSource for NodePattern {
    type Resolved = NodePattern;

    fn resolve(
        &self,
        node: &RsvgNode,
        draw_ctx: &mut DrawingCtx,
    ) -> Result<Self::Resolved, PaintServerError> {

        let mut result = node.borrow().get_impl::<NodePattern>().clone();
        *result.node.borrow_mut() = Some(node.clone());

        let mut stack = NodeStack::new();

        while !result.is_resolved() {
            if let Some(ref fallback) = result.fallback {
                if let Some(acquired) = draw_ctx
                    .acquired_nodes()
                    .get_node_of_type(fallback, NodeType::Pattern)
                {
                    let a_node = acquired.get();

                    if stack.contains(a_node) {
                        return Err(PaintServerError::CircularReference(fallback.clone()));
                    }

                    let node_data = a_node.borrow();

                    let fallback_pattern = node_data.get_impl::<NodePattern>();
                    *fallback_pattern.node.borrow_mut() = Some(a_node.clone());

                    result = result.resolve_from_fallback(fallback_pattern);

                    stack.push(a_node);
                } else {
                    result = result.resolve_from_defaults();
                }
            } else {
                result = result.resolve_from_defaults();
            }
        }

        Ok(result)
    }
}

impl ResolvedPaintSource for NodePattern {
    fn set_pattern_on_draw_context(
        self,
        values: &ComputedValues,
        draw_ctx: &mut DrawingCtx,
        _opacity: &UnitInterval,
        bbox: &BoundingBox,
    ) -> Result<bool, RenderingError> {
        assert!(self.is_resolved());

        if self.node.borrow().is_none() {
            // This means we didn't find any children among the fallbacks,
            // so there is nothing to render.
            return Ok(false);
        }

        let units = self.common.units.unwrap();
        let content_units = self.common.content_units.unwrap();
        let pattern_affine = self.common.affine.unwrap();
        let vbox = self.common.vbox.unwrap();
        let preserve_aspect_ratio = self.common.preserve_aspect_ratio.unwrap();

        let (pattern_x, pattern_y, pattern_width, pattern_height) = {
            let params = if units == PatternUnits(CoordUnits::ObjectBoundingBox) {
                draw_ctx.push_view_box(1.0, 1.0)
            } else {
                draw_ctx.get_view_params()
            };

            let pattern_x = self.common.x.unwrap().normalize(values, &params);
            let pattern_y = self.common.y.unwrap().normalize(values, &params);
            let pattern_width = self.common.width.unwrap().normalize(values, &params);
            let pattern_height = self.common.height.unwrap().normalize(values, &params);

            (pattern_x, pattern_y, pattern_width, pattern_height)
        };

        // Work out the size of the rectangle so it takes into account the object bounding box

        let bbwscale: f64;
        let bbhscale: f64;

        match units {
            PatternUnits(CoordUnits::ObjectBoundingBox) => {
                let bbrect = bbox.rect.unwrap();
                bbwscale = bbrect.width;
                bbhscale = bbrect.height;
            }

            PatternUnits(CoordUnits::UserSpaceOnUse) => {
                bbwscale = 1.0;
                bbhscale = 1.0;
            }
        }

        let cr = draw_ctx.get_cairo_context();
        let affine = cr.get_matrix();
        let taffine = cairo::Matrix::multiply(&pattern_affine, &affine);

        let mut scwscale = (taffine.xx * taffine.xx + taffine.xy * taffine.xy).sqrt();
        let mut schscale = (taffine.yx * taffine.yx + taffine.yy * taffine.yy).sqrt();

        let pw: i32 = (pattern_width * bbwscale * scwscale) as i32;
        let ph: i32 = (pattern_height * bbhscale * schscale) as i32;

        let scaled_width = pattern_width * bbwscale;
        let scaled_height = pattern_height * bbhscale;

        if scaled_width.abs() < f64::EPSILON
            || scaled_height.abs() < f64::EPSILON
            || pw < 1
            || ph < 1
        {
            return Ok(false);
        }

        scwscale = f64::from(pw) / scaled_width;
        schscale = f64::from(ph) / scaled_height;

        let mut affine: cairo::Matrix = cairo::Matrix::identity();

        // Create the pattern coordinate system
        match units {
            PatternUnits(CoordUnits::ObjectBoundingBox) => {
                let bbrect = bbox.rect.unwrap();
                affine.translate(
                    bbrect.x + pattern_x * bbrect.width,
                    bbrect.y + pattern_y * bbrect.height,
                );
            }

            PatternUnits(CoordUnits::UserSpaceOnUse) => {
                affine.translate(pattern_x, pattern_y);
            }
        }

        // Apply the pattern transform
        affine = cairo::Matrix::multiply(&affine, &pattern_affine);

        let mut caffine: cairo::Matrix;

        // Create the pattern contents coordinate system
        let _params = if let Some(vbox) = vbox {
            // If there is a vbox, use that
            let (mut x, mut y, w, h) = preserve_aspect_ratio.compute(
                &vbox,
                &cairo::Rectangle::new(
                    0.0,
                    0.0,
                    pattern_width * bbwscale,
                    pattern_height * bbhscale,
                ),
            );

            x -= vbox.x * w / vbox.width;
            y -= vbox.y * h / vbox.height;

            caffine = cairo::Matrix::new(w / vbox.width, 0.0, 0.0, h / vbox.height, x, y);

            draw_ctx.push_view_box(vbox.width, vbox.height)
        } else if content_units == PatternContentUnits(CoordUnits::ObjectBoundingBox) {
            // If coords are in terms of the bounding box, use them
            let bbrect = bbox.rect.unwrap();

            caffine = cairo::Matrix::identity();
            caffine.scale(bbrect.width, bbrect.height);

            draw_ctx.push_view_box(1.0, 1.0)
        } else {
            caffine = cairo::Matrix::identity();
            draw_ctx.get_view_params()
        };

        if !scwscale.approx_eq_cairo(1.0) || !schscale.approx_eq_cairo(1.0) {
            let mut scalematrix = cairo::Matrix::identity();
            scalematrix.scale(scwscale, schscale);
            caffine = cairo::Matrix::multiply(&caffine, &scalematrix);

            scalematrix = cairo::Matrix::identity();
            scalematrix.scale(1.0 / scwscale, 1.0 / schscale);

            affine = cairo::Matrix::multiply(&scalematrix, &affine);
        }

        // Draw to another surface

        let cr_save = draw_ctx.get_cairo_context();

        let surface = cr_save
            .get_target()
            .create_similar(cairo::Content::ColorAlpha, pw, ph);

        let cr_pattern = cairo::Context::new(&surface);

        draw_ctx.set_cairo_context(&cr_pattern);

        // Set up transformations to be determined by the contents units

        // Draw everything
        let pattern_node_borrow = self.node.borrow();
        let pattern_node = pattern_node_borrow.as_ref().unwrap();
        let pattern_cascaded = CascadedValues::new_from_node(pattern_node);
        let pattern_values = pattern_cascaded.get();

        cr_pattern.set_matrix(caffine);

        let res = draw_ctx.with_discrete_layer(&pattern_node, pattern_values, false, &mut |dc| {
            pattern_node.draw_children(&pattern_cascaded, dc, false)
        });

        // Return to the original coordinate system and rendering context

        draw_ctx.set_cairo_context(&cr_save);

        // Set the final surface as a Cairo pattern into the Cairo context

        let surface_pattern = cairo::SurfacePattern::create(&surface);
        surface_pattern.set_extend(cairo::Extend::Repeat);

        let mut matrix = affine;
        matrix.invert();

        surface_pattern.set_matrix(matrix);
        surface_pattern.set_filter(cairo::Filter::Best);

        cr_save.set_source(&surface_pattern);

        res.and_then(|_| Ok(true))
    }
}

impl NodePattern {
    fn is_resolved(&self) -> bool {
        self.common.units.is_some()
            && self.common.content_units.is_some()
            && self.common.vbox.is_some()
            && self.common.preserve_aspect_ratio.is_some()
            && self.common.affine.is_some()
            && self.common.x.is_some()
            && self.common.y.is_some()
            && self.common.width.is_some()
            && self.common.height.is_some()
            && self.children_are_resolved()
    }

    fn resolve_from_fallback(&self, fallback: &NodePattern) -> NodePattern {
        let units = self.common.units.or(fallback.common.units);
        let content_units = self.common.content_units.or(fallback.common.content_units);
        let vbox = self.common.vbox.or(fallback.common.vbox);
        let preserve_aspect_ratio = self.common.preserve_aspect_ratio.or(fallback.common.preserve_aspect_ratio);
        let affine = self.common.affine.or(fallback.common.affine);
        let x = self.common.x.or(fallback.common.x);
        let y = self.common.y.or(fallback.common.y);
        let width = self.common.width.or(fallback.common.width);
        let height = self.common.height.or(fallback.common.height);

        let node = if !self.children_are_resolved() {
            fallback.node.clone()
        } else {
            self.node.clone()
        };

        let fallback = fallback.fallback.clone();

        NodePattern {
            common: Common {
                units,
                content_units,
                vbox,
                preserve_aspect_ratio,
                affine,
                x,
                y,
                width,
                height,
            },
            node,
            fallback,
        }
    }

    fn resolve_from_defaults(&self) -> NodePattern {
        let units = self.common.units.or(Some(PatternUnits::default()));
        let content_units = self.common.content_units.or(Some(PatternContentUnits::default()));
        let vbox = self.common.vbox.or(Some(None));
        let preserve_aspect_ratio = self.common.preserve_aspect_ratio.or(Some(AspectRatio::default()));
        let affine = self.common.affine.or(Some(cairo::Matrix::identity()));
        let x = self.common.x.or(Some(Default::default()));
        let y = self.common.y.or(Some(Default::default()));
        let width = self.common.width.or(Some(Default::default()));
        let height = self.common.height.or(Some(Default::default()));
        let node = self.node.clone();
        let fallback = None;

        NodePattern {
            common: Common {
                units,
                content_units,
                vbox,
                preserve_aspect_ratio,
                affine,
                x,
                y,
                width,
                height,
            },
            node,
            fallback,
        }
    }

    fn children_are_resolved(&self) -> bool {
        if let Some(ref node) = *self.node.borrow() {
            node.has_children()
        } else {
            // We are an empty pattern; there is nothing further that
            // can be resolved for children.
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_resolved_from_defaults_is_really_resolved() {
        let pat = NodePattern::default();
        let res = pat.resolve_from_defaults();
        assert!(res.is_resolved());
    }
}
