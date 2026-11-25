use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Crop,
        Save,
        ScanText,
        Video,
        Film,
        CircleStop,
        Keyboard,
        Languages,
        Pin,
        Images,
        Square,
        ArrowUpRight,
        Minus,
        Pencil,
        Grid3x3,
        Type,
        ListOrdered,
        Undo2,
        Eraser,
        Trash,
        Stamp,
        ArrowUp,
        ArrowDown,
        ScrollText,
        ChevronsDown,
        PanelRight,
        RotateCcw,
        Scissors,
        ZoomIn,
        ZoomOut,
        Maximize2,
        Play,
        Pause,
        CircleX,
        Redo2,
        ImagePlus,
        Bold,
        Sun,
        Moon,
        Grid2x2,
        AlignStartVertical,
        AlignCenterVertical,
        AlignCenterHorizontal,
        AlignHorizontalSpaceBetween,
        AlignVerticalSpaceBetween
    ]
);

/// The default component icons plus the few extra Lucide icons SnapIt uses.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        Ok(paths)
    }
}
