use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    NoDevice,
    Unsupported,
    NotObserved,
    MissingCalibration,
    Ambiguous,
    Blind,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum Evidence<T> {
    Observed(T),
    Uncertain { value: T, reason: UnavailableReason },
    Unavailable(UnavailableReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalPoint {
    pub x: i32,
    pub y: i32,
}

/// Coordinates are physical desktop pixels with exclusive right and bottom edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RectEdges", into = "RectEdges")]
pub struct PhysicalRect(RectEdges);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct RectEdges {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl PhysicalRect {
    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Option<Self> {
        RectEdges { left, top, right, bottom }.try_into().ok()
    }

    pub fn edges(self) -> (i32, i32, i32, i32) {
        (self.0.left, self.0.top, self.0.right, self.0.bottom)
    }

    pub fn contains(self, point: PhysicalPoint) -> bool {
        let (left, top, right, bottom) = self.edges();
        (left..right).contains(&point.x) && (top..bottom).contains(&point.y)
    }
}

impl TryFrom<RectEdges> for PhysicalRect {
    type Error = &'static str;

    fn try_from(edges: RectEdges) -> Result<Self, Self::Error> {
        if edges.left >= edges.right || edges.top >= edges.bottom {
            return Err("Physical rectangle must have positive width and height.");
        }
        Ok(Self(edges))
    }
}

impl From<PhysicalRect> for RectEdges {
    fn from(rect: PhysicalRect) -> Self {
        rect.0
    }
}
