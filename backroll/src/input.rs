use crate::{BackrollError, Frame, PlayerHandle, MAX_PLAYERS, MAX_ROLLBACK_FRAMES};
use std::convert::TryFrom;
use tracing::debug;

#[inline]
fn previous_frame(offset: usize) -> usize {
    if offset == 0 {
        MAX_ROLLBACK_FRAMES - 1
    } else {
        offset - 1
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameInput<T> {
    pub frame: Frame,
    pub input: T,
}

impl<T: bytemuck::Zeroable> Default for FrameInput<T> {
    fn default() -> Self {
        Self {
            frame: super::NULL_FRAME,
            input: T::zeroed(),
        }
    }
}

impl<T: bytemuck::Zeroable> FrameInput<T> {
    pub fn clear(&mut self) {
        self.input = T::zeroed();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A container of inputs for all of the players for single frame of a game.
pub struct GameInput<T> {
    /// The frame number the inputs correspond to.
    pub frame: Frame,
    pub(crate) disconnected: u8,
    pub(crate) inputs: [T; MAX_PLAYERS],
}

impl<T: bytemuck::Zeroable> Default for GameInput<T> {
    fn default() -> Self {
        Self {
            frame: super::NULL_FRAME,
            disconnected: 0,
            inputs: unsafe { core::mem::zeroed() },
        }
    }
}

impl<T: bytemuck::Zeroable> GameInput<T> {
    /// Gets the input for a specific player. Returns [InvalidPlayer]
    /// if the provided player handle does not correspond to a valid player.
    ///
    /// [InvalidPlayer]: crate::BackrollError::InvalidPlayer
    pub fn get(&self, player: PlayerHandle) -> Result<&T, BackrollError> {
        if player.0 >= MAX_PLAYERS {
            return Err(BackrollError::InvalidPlayer(player));
        }
        Ok(&self.inputs[player.0])
    }

    /// Checks if a given player is currently disconnected. Returns [InvalidPlayer]
    /// if the provided player handle does not correspond to a valid player.
    ///
    /// [InvalidPlayer]: crate::BackrollError::InvalidPlayer
    pub fn is_disconnected(&self, player: PlayerHandle) -> Result<bool, BackrollError> {
        if player.0 >= MAX_PLAYERS {
            return Err(BackrollError::InvalidPlayer(player));
        }
        Ok(self.disconnected & (1 << player.0) != 0)
    }
}

pub enum FetchedInput<T> {
    Normal(FrameInput<T>),
    Prediction(FrameInput<T>),
}

impl<T> FetchedInput<T> {
    pub fn unwrap(self) -> FrameInput<T> {
        match self {
            Self::Normal(input) => input,
            Self::Prediction(input) => input,
        }
    }

    pub fn is_prediction(&self) -> bool {
        matches!(self, Self::Prediction(_))
    }
}

pub struct InputQueue<T> {
    head: usize,
    tail: usize,
    length: usize,
    first_frame: bool,

    last_user_added_frame: Frame,
    last_added_frame: Frame,
    first_incorrect_frame: Frame,
    last_frame_requested: Frame,

    frame_delay: Frame,

    inputs: [FrameInput<T>; MAX_ROLLBACK_FRAMES],
    prediction: FrameInput<T>,
}

impl<T: bytemuck::Zeroable + Clone + PartialEq> InputQueue<T> {
    #[allow(clippy::uninit_assumed_init)]
    pub fn new(frame_delay: Frame) -> Self {
        // This is necessary as Default is not defined on arrays of more
        // than 32 without a Copy trait bound.
        //
        // SAFE: The entire buffer is initialized by the end of the for-loop.
        // Assuming Zeroable is implemented correctly, this should also never
        // panic, so a buffer will always correctly be allocated as a large
        // zeroed buffer.
        let inputs: [FrameInput<T>; MAX_ROLLBACK_FRAMES] = {
            let mut inputs: [FrameInput<T>; MAX_ROLLBACK_FRAMES] =
                unsafe { std::mem::MaybeUninit::uninit().assume_init() };

            for input in inputs.iter_mut() {
                *input = Default::default();
            }

            inputs
        };

        Self {
            head: 0,
            tail: 0,
            length: 0,
            frame_delay,
            first_frame: true,
            last_user_added_frame: super::NULL_FRAME,
            first_incorrect_frame: super::NULL_FRAME,
            last_frame_requested: super::NULL_FRAME,
            last_added_frame: super::NULL_FRAME,
            inputs,
            prediction: Default::default(),
        }
    }

    pub fn last_confirmed_frame(&self) -> Frame {
        debug!("returning last confirmed frame {}.", self.last_added_frame);
        self.last_added_frame
    }

    pub fn first_incorrect_frame(&self) -> Frame {
        self.first_incorrect_frame
    }

    pub fn set_frame_delay(&mut self, frame_delay: Frame) {
        debug_assert!(!super::is_null(frame_delay));
        self.frame_delay = frame_delay;
    }

    pub fn discard_confirmed_frames(&mut self, mut frame: Frame) {
        debug_assert!(!super::is_null(frame));
        if super::is_null(self.last_frame_requested) {
            frame = std::cmp::min(frame, self.last_frame_requested)
        }

        debug!(
            "discarding confirmed frames up to {} (last_added:{} length:{}).",
            frame, self.last_added_frame, self.length
        );
        if frame >= self.last_added_frame {
            self.tail = self.head;
            self.length = 0;
        } else {
            let offset = frame - self.inputs[self.tail].frame + 1;
            let offset = usize::try_from(offset).unwrap();

            debug!("difference of {} frames.", offset);

            self.tail = (self.tail + offset) % MAX_ROLLBACK_FRAMES;
            self.length -= offset;
        }
    }

    pub fn reset_prediction(&mut self, frame: Frame) {
        debug_assert!(
            super::is_null(self.first_incorrect_frame) || frame <= self.first_incorrect_frame
        );

        debug!("resetting all prediction errors back to frame {}.", frame);

        // There's nothing really to do other than reset our prediction
        // state and the incorrect frame counter...
        self.prediction.frame = super::NULL_FRAME;
        self.first_incorrect_frame = super::NULL_FRAME;
        self.last_frame_requested = super::NULL_FRAME;
    }

    pub fn get_confirmed_input(&self, frame: Frame) -> Option<&FrameInput<T>> {
        debug_assert!(
            super::is_null(self.first_incorrect_frame) || frame < self.first_incorrect_frame
        );
        let offset = usize::try_from(frame).unwrap() % MAX_ROLLBACK_FRAMES;
        self.inputs.get(offset)
    }

    pub fn get_input(&mut self, frame: Frame) -> FetchedInput<T> {
        debug!("requesting input frame {:?}.", frame);

        // No one should ever try to grab any input when we have a prediction
        // error. Doing so means that we're just going further down the wrong
        // path. Assert this to verify that it's true.
        debug_assert!(super::is_null(self.first_incorrect_frame));

        // Remember the last requested frame number for later.  We'll need
        // this in add_input() to drop out of prediction mode.
        self.last_frame_requested = frame;
        debug_assert!(frame >= self.inputs[self.tail].frame);

        if super::is_null(self.prediction.frame) {
            // If the frame requested is in our range, fetch it out of the queue and
            // return it.
            let offset = frame - self.inputs[self.tail].frame;
            let mut offset = usize::try_from(offset).unwrap();
            if offset < self.len() {
                offset = (offset + self.tail) % MAX_ROLLBACK_FRAMES;
                let input = self.inputs[offset].clone();
                debug_assert!(input.frame == frame);
                debug!("returning confirmed frame number {}.", input.frame);
                return FetchedInput::Normal(input);
            }

            // The requested frame isn't in the queue.  Bummer.  This means we need
            // to return a prediction frame.  Predict that the user will do the
            // same thing they did last time.
            if frame == 0 {
                debug!("basing new prediction frame from nothing, you're client wants frame 0.");
                self.prediction.clear();
            } else if super::is_null(self.last_added_frame) {
                debug!("basing new prediction frame from nothing, since we have no frames yet.");
                self.prediction.clear();
            } else {
                debug!(
                    "basing new prediction frame from previously added frame (frame: {}).",
                    self.inputs[previous_frame(self.head)].frame
                );
                self.prediction = self.inputs[previous_frame(self.head)].clone();
            }
            self.prediction.frame += 1;
        }

        // If we've made it this far, we must be predicting.  Go ahead and
        // forward the prediction frame contents.  Be sure to return the
        // frame number requested by the client, though.
        let mut prediction = self.prediction.clone();
        prediction.frame = frame;
        debug!(
            "returning prediction frame number {} ({}).",
            frame, self.prediction.frame
        );
        FetchedInput::Prediction(prediction)
    }

    pub fn add_input(&mut self, input: FrameInput<T>) -> Frame {
        // These next two lines simply verify that inputs are passed in
        // sequentially by the user, regardless of frame delay.
        debug_assert!(
            super::is_null(self.last_user_added_frame)
                || input.frame == self.last_user_added_frame + 1
        );
        self.last_user_added_frame = input.frame;
        debug!("adding input frame number {} to queue.", input.frame);

        // Move the queue head to the correct point in preparation to
        // input the frame into the queue.
        let new_frame = self.advance_queue_head(input.frame);
        if !super::is_null(new_frame) {
            self.add_delayed_input(new_frame, input);
        }

        // Update the frame number for the input. This will also set the
        // frame to NULL_FRAME for frames that get dropped (by design).
        new_frame
    }

    fn add_delayed_input(&mut self, frame: Frame, input: FrameInput<T>) {
        debug!("adding delayed input frame number {} to queue.", frame);
        debug_assert!(super::is_null(self.last_added_frame) || frame == self.last_added_frame + 1);
        debug_assert!(frame == 0 || self.inputs[previous_frame(self.head)].frame == frame - 1);

        // Add the frame to the back of the queue
        self.inputs[self.head] = input.clone();
        self.inputs[self.head].frame = frame;
        self.head = (self.head + 1) % MAX_ROLLBACK_FRAMES;
        self.length += 1;
        self.first_frame = false;
        self.last_added_frame = frame;

        if !super::is_null(self.prediction.frame) {
            debug_assert!(frame == self.prediction.frame);
            // We've been predicting...  See if the inputs we've gotten match
            // what we've been predicting.  If so, don't worry about it.  If not,
            // remember the first input which was incorrect so we can report it
            // in first_incorrect_frame()
            if super::is_null(self.first_incorrect_frame) && self.prediction != input {
                debug!("frame {} does not match prediction. marking error.", frame);
                self.first_incorrect_frame = frame;
            }

            // If this input is the same frame as the last one requested and we
            // still haven't found any mis-predicted inputs, we can dump out
            // of predition mode entirely!  Otherwise, advance the prediction frame
            // count up.
            if self.prediction.frame == self.last_frame_requested
                && super::is_null(self.first_incorrect_frame)
            {
                debug!("prediction is correct! dumping out of prediction mode.");
                self.prediction.frame = super::NULL_FRAME;
            } else {
                self.prediction.frame += 1;
            }
        }
        debug_assert!(self.len() <= MAX_ROLLBACK_FRAMES);
    }

    fn advance_queue_head(&mut self, mut frame: Frame) -> Frame {
        debug!("advancing queue head to frame {}.", frame);
        let mut expected_frame = if self.first_frame {
            0
        } else {
            self.inputs[previous_frame(self.head)].frame + 1
        };
        frame += self.frame_delay;

        if expected_frame > frame {
            // This can occur when the frame delay has dropped since the last
            // time we shoved a frame into the system.  In this case, there's
            // no room on the queue.  Toss it.
            debug!(
                "Dropping input frame {} (expected next frame to be {}).",
                frame, expected_frame
            );
            return super::NULL_FRAME;
        }

        while expected_frame < frame {
            // This can occur when the frame delay has been increased since the last
            // time we shoved a frame into the system.  We need to replicate the
            // last frame in the queue several times in order to fill the space
            // left.
            debug!(
                "Adding padding frame {} to account for change in frame delay.",
                expected_frame
            );
            self.add_delayed_input(
                expected_frame,
                self.inputs[previous_frame(self.head)].clone(),
            );
            expected_frame += 1;
        }

        debug_assert!(frame == 0 || frame == self.inputs[previous_frame(self.head)].frame + 1);
        frame
    }

    pub fn len(&self) -> usize {
        self.length
    }
}
