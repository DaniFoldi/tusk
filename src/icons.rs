use std::sync::{Arc, LazyLock};

use gpui_kit::{ImageSource, Img, RenderImage, Styled as _, img, px};
use image::Frame;
use smallvec::smallvec;

/// Object-tree icon kinds.
#[derive(Clone, Copy)]
#[allow(dead_code)] // Key lands with PK badges next
pub enum DbIcon {
    Database,
    Postgres,
    Table,
    View,
    Sql,
    Key,
    SchemaFolder,
    TablesFolder,
    ViewsFolder,
    FunctionsFolder,
}

fn decode(bytes: &'static [u8]) -> Arc<RenderImage> {
    let mut bgra = image::load_from_memory(bytes)
        .expect("bundled icon png must decode")
        .to_rgba8();
    for px in bgra.pixels_mut() {
        px.0.swap(0, 2);
    }
    Arc::new(RenderImage::new(smallvec![Frame::new(bgra)]))
}

macro_rules! icon_static {
    ($name:ident, $file:literal) => {
        static $name: LazyLock<Arc<RenderImage>> = LazyLock::new(|| decode(include_bytes!($file)));
    };
}

icon_static!(DATABASE, "../assets/icons/database.png");
icon_static!(POSTGRES, "../assets/icons/postgresql.png");
icon_static!(TABLE, "../assets/icons/table_grid.png");
icon_static!(VIEW, "../assets/icons/view_grid.png");
icon_static!(SQL, "../assets/icons/sql.png");
icon_static!(KEY, "../assets/icons/key.png");
icon_static!(SCHEMA_FOLDER, "../assets/icons/folder_schema.png");
icon_static!(TABLES_FOLDER, "../assets/icons/folder_database.png");
icon_static!(VIEWS_FOLDER, "../assets/icons/folder_views.png");
icon_static!(FUNCTIONS_FOLDER, "../assets/icons/folder_functions.png");

impl DbIcon {
    fn image(self) -> Arc<RenderImage> {
        match self {
            DbIcon::Database => DATABASE.clone(),
            DbIcon::Postgres => POSTGRES.clone(),
            DbIcon::Table => TABLE.clone(),
            DbIcon::View => VIEW.clone(),
            DbIcon::Sql => SQL.clone(),
            DbIcon::Key => KEY.clone(),
            DbIcon::SchemaFolder => SCHEMA_FOLDER.clone(),
            DbIcon::TablesFolder => TABLES_FOLDER.clone(),
            DbIcon::ViewsFolder => VIEWS_FOLDER.clone(),
            DbIcon::FunctionsFolder => FUNCTIONS_FOLDER.clone(),
        }
    }

    fn img_px(self, size: f32) -> Img {
        img(ImageSource::Render(self.image()))
            .w(px(size))
            .h(px(size))
    }

    /// 16px full-color icon for rows, tabs and menus.
    pub fn icon(self) -> Img {
        self.img_px(16.)
    }

    /// Arbitrary-size full-color icon (e.g. the 40px welcome logo).
    pub fn icon_px(self, size: f32) -> Img {
        self.img_px(size)
    }
}

/// Engine logo (monochrome SVG), when one is bundled.
pub fn engine_logo(e: crate::engine::Engine) -> Option<&'static [u8]> {
    use crate::engine::Engine as E;
    Some(match e {
        E::Postgres => include_bytes!("../assets/icons/engines/postgres.svg"),
        E::MySql => include_bytes!("../assets/icons/engines/mysql.svg"),
        E::MariaDb => include_bytes!("../assets/icons/engines/mariadb.svg"),
        E::Sqlite => include_bytes!("../assets/icons/engines/sqlite.svg"),
        E::MsSql => include_bytes!("../assets/icons/engines/mssql.svg"),
        E::Redis => include_bytes!("../assets/icons/engines/redis.svg"),
        E::Valkey => include_bytes!("../assets/icons/engines/valkey.svg"),
        E::Cassandra => include_bytes!("../assets/icons/engines/cassandra.svg"),
        E::MongoDb => include_bytes!("../assets/icons/engines/mongodb.svg"),
        E::Oracle => include_bytes!("../assets/icons/engines/oracle.svg"),
        E::Redshift => include_bytes!("../assets/icons/engines/redshift.svg"),
        E::Cockroach => include_bytes!("../assets/icons/engines/cockroach.svg"),
        E::Snowflake => include_bytes!("../assets/icons/engines/snowflake.svg"),
        E::BigQuery => include_bytes!("../assets/icons/engines/bigquery.svg"),
        E::DuckDb => include_bytes!("../assets/icons/engines/duckdb.svg"),
        E::ClickHouse => include_bytes!("../assets/icons/engines/clickhouse.svg"),
        E::DynamoDb => include_bytes!("../assets/icons/engines/dynamodb.svg"),
        E::LibSql => include_bytes!("../assets/icons/engines/libsql.svg"),
        E::CloudflareD1 => include_bytes!("../assets/icons/engines/d1.svg"),
        E::Vertica | E::Greenplum => return None,
    })
}

/// A colored rounded tile with the engine's logo (or its two letters).
pub fn engine_badge(e: crate::engine::Engine, side: f32) -> gpui_kit::AnyElement {
    use gpui_kit::*;
    let tile = div()
        .size(px(side))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(side * 0.22))
        .bg(rgb(e.color()))
        .text_color(rgb(0xFFFFFF));
    match engine_logo(e) {
        Some(bytes) => tile
            .child(
                gpui_kit::component::Icon::default()
                    .data(bytes)
                    .size(px(side * 0.58)),
            )
            .into_any_element(),
        None => tile
            .text_size(px(side * 0.42))
            .font_weight(FontWeight::BOLD)
            .child(e.abbr())
            .into_any_element(),
    }
}
