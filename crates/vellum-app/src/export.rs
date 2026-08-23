//! The desktop's half of the board exporter.
//!
//! The adapter moved to [`vellum_project::export`] so the browser can share it, and is
//! re-exported here under its old path exactly as `project`, `connector` and `theme` were —
//! so nothing that says `crate::export::` had to move, and there is still one derivation of
//! how a board becomes a page. What stays behind is the one thing that cannot cross:
//! [`vellum_store::BlobStore`], which is SQLite-adjacent and native-only.

pub use vellum_project::export::{
    BundledFonts, ExportAssets, NoAssets, document_params, raster_request, raster_scale,
};

use vellum_export::ImageData;
use vellum_store::BlobStore;

/// `BoardExport` under its old name, with the desktop's answer to the seam already chosen.
///
/// An alias rather than a wrapper: `BoardExport::new` takes `impl Into<A>`, so pinning `A`
/// here is what keeps `actions.rs`'s existing call compiling untouched.
pub type BoardExport<'a> = vellum_project::export::BoardExport<'a, BlobAssets<'a>>;

/// The content-addressed blob store, as an export asset source.
///
/// A newtype because the orphan rule forbids `impl ExportAssets for BlobStore` — both types
/// are foreign here — and because the borrow has to live somewhere now that `BoardExport`
/// holds its assets by value.
#[derive(Debug, Clone, Copy)]
pub struct BlobAssets<'a>(pub &'a BlobStore);

impl<'a> From<&'a BlobStore> for BlobAssets<'a> {
    fn from(blobs: &'a BlobStore) -> Self {
        Self(blobs)
    }
}

impl ExportAssets for BlobAssets<'_> {
    fn image(&self, asset_id: &str) -> Option<ImageData> {
        let hash = asset_id.parse().ok()?;
        match self.0.get(&hash) {
            Ok(Some(bytes)) => {
                // The media type from the bytes themselves, since the blob store is
                // content-addressed and holds no metadata beside them. SVG needs it for the
                // data URI's prefix and PDF for the filter it picks.
                let media = image::guess_format(&bytes)
                    .map_or("application/octet-stream", |format| format.to_mime_type());
                Some(ImageData::encoded(media, bytes))
            }
            Ok(None) => {
                log::warn!("exporting: asset {asset_id} is not in the blob store");
                None
            }
            Err(error) => {
                log::warn!("exporting asset {asset_id}: {error}");
                None
            }
        }
    }

    // No `font`, deliberately: the desktop's PDFs have always fallen back to a standard face,
    // and supplying `BundledFonts` here would change what every existing export embeds. It is
    // one line the day somebody asks for it.
}
