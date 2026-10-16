//! Reading the screen (card R2-07, the first part): a picture of the screen taken in memory, the text in it read by the text reader that comes with Windows, and two
//! tools for the agent over that. No model and no download. What is not here: describing a picture or finding a control that has no text (that needs a model that
//! reads images), the text of a window that another window covers, and merging the text with the control tree of the Windows tools.

pub mod capture;
pub mod find;
pub mod geometry;
pub mod ocr;
pub mod provider;

pub use capture::{capture, is_aqua_program, Target};
pub use find::{find, Line, Match, Reading, Word};
pub use geometry::{Rect, Shot};
pub use provider::{ScreenProvider, ScreenSource, WindowsScreen};
