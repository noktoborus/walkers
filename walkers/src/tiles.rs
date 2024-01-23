use std::collections::hash_map::Entry;
use std::collections::HashMap;

use egui::{pos2, Color32, Context, Mesh, Rect, Vec2};
use egui::{ColorImage, TextureHandle};
use image::ImageError;

use crate::download::{download_continuously, HttpOptions};
use crate::io::Runtime;
use crate::mercator::TileId;
use crate::sources::{Attribution, TileSource};

pub(crate) fn rect(screen_position: Vec2, tile_size: u32) -> Rect {
    Rect::from_min_size(screen_position.to_pos2(), Vec2::splat(tile_size as f32))
}

#[derive(Clone)]
pub struct Texture(TextureHandle);

impl Texture {
    pub fn new(image: &[u8], ctx: &Context) -> Result<Self, ImageError> {
        let image = image::load_from_memory(image)?.to_rgba8();
        let pixels = image.as_flat_samples();
        let image = ColorImage::from_rgba_unmultiplied(
            [image.width() as _, image.height() as _],
            pixels.as_slice(),
        );

        Ok(Self::from_color_image(image, ctx))
    }

    /// Load the texture from egui's [`ColorImage`].
    pub fn from_color_image(color_image: ColorImage, ctx: &Context) -> Self {
        Self(ctx.load_texture("image", color_image, Default::default()))
    }

    pub(crate) fn size(&self) -> Vec2 {
        self.0.size_vec2()
    }

    pub(crate) fn mesh(&self, screen_position: Vec2, tile_size: u32) -> Mesh {
        self.mesh_with_rect(rect(screen_position, tile_size))
    }

    pub(crate) fn mesh_with_rect(&self, rect: Rect) -> Mesh {
        let mut mesh = Mesh::with_texture(self.0.id());
        mesh.add_rect_with_uv(
            rect,
            Rect::from_min_max(pos2(0., 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        mesh
    }
}

pub trait TilesManager {
    fn at(&mut self, tile_id: TileId) -> Option<Texture>;
    fn attribution(&self) -> Attribution;
    fn tile_size(&self) -> u32;
    fn available_zoom(&self) -> Vec<u8>;
}

/// Downloads the tiles via HTTP. It must persist between frames.
pub struct Tiles {
    attribution: Attribution,

    cache: HashMap<TileId, Option<Texture>>,

    /// Tiles to be downloaded by the IO thread.
    request_tx: futures::channel::mpsc::Sender<TileId>,

    /// Tiles that got downloaded and should be put in the cache.
    tile_rx: futures::channel::mpsc::Receiver<(TileId, Texture)>,

    #[allow(dead_code)] // Significant Drop
    runtime: Runtime,

    tile_size: u32,
}

impl Tiles {
    /// Construct new [`Tiles`] with default [`HttpOptions`].
    pub fn new<S>(source: S, egui_ctx: Context) -> Self
    where
        S: TileSource + Send + 'static,
    {
        Self::with_options(source, HttpOptions::default(), egui_ctx)
    }

    /// Construct new [`Tiles`] with supplied [`HttpOptions`].
    pub fn with_options<S>(source: S, http_options: HttpOptions, egui_ctx: Context) -> Self
    where
        S: TileSource + Send + 'static,
    {
        // Minimum value which didn't cause any stalls while testing.
        let channel_size = 20;

        let (request_tx, request_rx) = futures::channel::mpsc::channel(channel_size);
        let (tile_tx, tile_rx) = futures::channel::mpsc::channel(channel_size);
        let attribution = source.attribution();
        let tile_size = source.tile_size();

        let runtime = Runtime::new(download_continuously(
            source,
            http_options,
            request_rx,
            tile_tx,
            egui_ctx,
        ));

        Self {
            attribution,
            cache: Default::default(),
            request_tx,
            tile_rx,
            runtime,
            tile_size,
        }
    }
}

impl TilesManager for Tiles {
    /// Attribution of the source this tile cache pulls images from. Typically,
    /// this should be displayed somewhere on the top of the map widget.
    fn attribution(&self) -> Attribution {
        self.attribution.clone()
    }

    /// Return a tile if already in cache, schedule a download otherwise.
    fn at(&mut self, tile_id: TileId) -> Option<Texture> {
        // Just take one at the time.
        match self.tile_rx.try_next() {
            Ok(Some((tile_id, tile))) => {
                self.cache.insert(tile_id, Some(tile));
            }
            Err(_) => {
                // Just ignore. It means that no new tile was downloaded.
            }
            Ok(None) => {
                log::error!("IO thread is dead")
            }
        }

        match self.cache.entry(tile_id) {
            Entry::Occupied(entry) => entry.get().clone(),
            Entry::Vacant(entry) => {
                if let Ok(()) = self.request_tx.try_send(tile_id) {
                    log::debug!("Requested tile: {:?}", tile_id);
                    entry.insert(None);
                } else {
                    log::debug!("Request queue is full.");
                }
                None
            }
        }
    }

    fn tile_size(&self) -> u32 {
        self.tile_size
    }

    fn available_zoom(&self) -> Vec<u8> {
        return Vec::from_iter(0..=19);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermocker::{Bytes, StatusCode};
    use std::time::Duration;

    static TILE_ID: TileId = TileId {
        x: 1,
        y: 2,
        zoom: 3,
    };

    struct TestSource {
        base_url: String,
    }

    impl TestSource {
        pub fn new(base_url: String) -> Self {
            Self { base_url }
        }
    }

    impl TileSource for TestSource {
        fn tile_url(&self, tile_id: TileId) -> String {
            format!(
                "{}/{}/{}/{}.png",
                self.base_url, tile_id.zoom, tile_id.x, tile_id.y
            )
        }

        fn attribution(&self) -> Attribution {
            Attribution {
                text: "",
                url: "",
                logo_light: None,
                logo_dark: None,
            }
        }
    }

    /// Creates [`hypermocker::Mock`], and function mapping `TileId` to its URL.
    async fn hypermocker_mock() -> (hypermocker::Server, TestSource) {
        let server = hypermocker::Server::bind().await;
        let url = format!("http://localhost:{}", server.port());
        (server, TestSource::new(url))
    }

    async fn assert_tile_to_become_available_eventually(tiles: &mut Tiles, tile_id: TileId) {
        log::info!("Waiting for {:?} to become available.", tile_id);
        while tiles.at(tile_id).is_none() {
            // Need to yield to the runtime for things to move.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn download_single_tile() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        server
            .anticipate("/3/1/2.png")
            .await
            .respond(include_bytes!("../assets/blank-255-tile.png"))
            .await;

        let mut tiles = Tiles::new(source, Context::default());

        // First query start the download, but it will always return None.
        assert!(tiles.at(TILE_ID).is_none());

        // Eventually it gets downloaded and become available in cache.
        assert_tile_to_become_available_eventually(&mut tiles, TILE_ID).await;
    }

    #[tokio::test]
    async fn there_can_be_6_simultaneous_downloads_at_most() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = Tiles::new(source, Context::default());

        // First download is started immediately.
        let mut first_outstanding_request = server.anticipate(format!("/3/1/2.png")).await;
        assert!(tiles.at(TILE_ID).is_none());
        first_outstanding_request.expect().await;

        let tile_ids: Vec<_> = (2..7).map(|x| TileId { x, y: 1, zoom: 1 }).collect();

        // Rest of the downloads are started right away too, but they remain active.
        let mut requests = Vec::new();
        for tile_id in tile_ids {
            let mut request = server.anticipate(format!("/1/{}/1.png", tile_id.x)).await;
            assert!(tiles.at(tile_id).is_none());
            request.expect().await;
            requests.push(request);
        }

        // Last download is NOT started, because we are at the limit of concurrent downloads.
        assert!(tiles
            .at(TileId {
                x: 99,
                y: 99,
                zoom: 1
            })
            .is_none());

        // Make sure it does not come.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Last download will start as soon as one of the previous ones are responded to.
        let mut awaiting_request = server.anticipate("/1/99/99.png".to_string()).await;

        first_outstanding_request
            .respond(Bytes::from_static(include_bytes!(
                "../assets/blank-255-tile.png"
            )))
            .await;

        awaiting_request.expect().await;
    }

    async fn assert_tile_is_empty_forever(tiles: &mut Tiles) {
        // Should be None now, and forever.
        assert!(tiles.at(TILE_ID).is_none());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(tiles.at(TILE_ID).is_none());
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_returns_error() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = Tiles::new(source, Context::default());
        server
            .anticipate("/3/1/2.png")
            .await
            .respond_with_status(StatusCode::NOT_FOUND)
            .await;

        assert_tile_is_empty_forever(&mut tiles).await;
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_returns_no_body() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = Tiles::new(source, Context::default());
        server
            .anticipate("/3/1/2.png")
            .await
            .respond_with_status(StatusCode::OK)
            .await;

        assert_tile_is_empty_forever(&mut tiles).await;
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_returns_garbage() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = Tiles::new(source, Context::default());
        server
            .anticipate("/3/1/2.png")
            .await
            .respond("definitely not an image")
            .await;

        assert_tile_is_empty_forever(&mut tiles).await;
    }

    /// Tile source, which gives invalid urls.
    struct GarbageSource;

    impl TileSource for GarbageSource {
        fn tile_url(&self, _: TileId) -> String {
            "totally invalid url".to_string()
        }

        fn attribution(&self) -> Attribution {
            Attribution {
                text: "",
                url: "",
                logo_light: None,
                logo_dark: None,
            }
        }
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_can_not_even_connect() {
        let _ = env_logger::try_init();
        let mut tiles = Tiles::new(GarbageSource, Context::default());
        assert_tile_is_empty_forever(&mut tiles).await;
    }
}
