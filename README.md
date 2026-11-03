# mcsr-inv-cut

Finds the parts of an MCSR recording where you have the inventory, a chest, a crafting table, a furnace and so on open. It tells you how long you spent in them, and can export the video with those parts cut out.

Pause and settings screens are counted separately ("other GUI"), not as inventory time. If the detection gets something wrong, you can fix it on the timeline.

The UI is in Japanese for now.

## Usage

You need ffmpeg and ffprobe on your PATH. I've only run it on Windows.

```
cargo build --release
target/release/mcsr-inv-cut <video>
```

Open a video (drag and drop works too) and press 解析する (analyze). A 15-minute 1080p60 video takes around 2-3 minutes.

If the recording has a webcam or chat on top of the game, like a stream VOD, use 解析範囲を描く (draw analysis area) first and box just the game. That cuts down on false hits.

When it's done, the times show up on the right. To fix a mistake, select a range on the timeline (Shift+drag or double-click) and press a number key.

| Key | |
|---|---|
| `1` / `2` / `3` | Mark as container GUI / other GUI / gameplay |
| `0` | Back to auto detection |
| `←` `→` | Move one frame (with Shift, jump to the next segment edge) |
| `I` `O` | Set the start / end of the measured range |

Results and your fixes are saved automatically next to the video as `<video>.mcsrcut.json`.

### CLI

You can also use it without the GUI.

```
mcsr-inv-cut analyze <video> [--roi x0,y0,x1,y1]
mcsr-inv-cut report <video>
mcsr-inv-cut csv <video> <out.csv>
mcsr-inv-cut cut <video> <out.mp4> [--mode remove-container|remove-gui|only-container] [--crf N] [--preset P]
```

## How it detects things

When you open an inventory-type screen, a gray (#C6C6C6) panel shows up in the middle and the background gets darker. So for each frame it looks for blobs of that color. A blob counts as a container GUI if it's fairly large, doesn't touch the edge of the frame, has a straight edge both horizontally and vertically, and the area around it is dark. It only asks for one straight edge in each direction because item tooltips can cover the panel's edges.

Pause menus and the like don't have the panel, but the whole screen darkens and the red of the hearts goes away. Frames with no panel and almost no heart red are counted as other GUI.

See `src/detect.rs` for the details.

## When it doesn't work

- Texture packs that change the GUI color won't be picked up
- A very small GUI scale might not be picked up
- If the hearts go off-screen, e.g. with the tall resize used for eye measuring, normal gameplay gets counted as other GUI. This doesn't affect the container GUI time

My notes from testing it are in [docs/notes.md](docs/notes.md) (in Japanese).
