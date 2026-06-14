# Roamer 角色包生成管线:把生成图(sheet 或散帧)归一化成标准素材
#
# 规范(代码按此假设,勿单独改一处):
#   - 每角色 6 张:idle + run 共 6 帧(文件名顺序 = 播放顺序)
#     idle 单帧:<name>-idle.png;idle 多帧(悬停浮动等):<name>-idle-1..N.png
#     run/飞行:<name>-run-1..M.png
#   - 画布 192x192 透明底,角色朝右
#   - 各帧身体(不透明像素)面积归一到中位数 -> 状态切换体量不变
#   - 质心居中;run 帧保留竖向弹跳偏移(相对各帧均值,clamp ±14px),idle 帧不加偏移
#
# 用法:
#   整张 sheet(2x3,默认第 1 格停止、2-6 格跑动):
#     python3 scripts/make-roamer-frames.py --sheet 生成图.png --name cat
#   多帧 idle(如悬浮机器人:1-2 格悬停、3-6 格飞行):
#     python3 scripts/make-roamer-frames.py --sheet 图.png --name sentinel --idle-cells 1,2
#   网格里跑动顺序不对时用 --run-order 指定(格子序号 1 基,不含 idle 格):
#     python3 scripts/make-roamer-frames.py --sheet 图.png --name cat --run-order 3,4,6,5,2
#   已抠好的散帧(前 --idle-count 张是 idle,默认 1,其余按播放顺序):
#     python3 scripts/make-roamer-frames.py --frames idle.png,r1.png,r2.png,r3.png,r4.png,r5.png --name dog
#
# 依赖:pillow numpy scipy rembg(rembg 只在 --sheet 模式抠背景时需要;
#   装 rembg 会把 numpy/scipy 拉到新版,装完跑一句
#   pip3 install --user 'numpy<2' 'scipy==1.13.1' 还原环境钉子,rembg 兼容)

import argparse
import numpy as np
from PIL import Image

CANVAS = 192
MARGIN = 4
BOUNCE_MAX = 14
# 体量基准:屏显面积约 1076px^2(旺财首发尺寸),据此打印建议显示宽高
TARGET_SCREEN_AREA = 1076.0


def alpha_stats(arr):
    a = arr[..., 3] > 24
    ys, xs = np.where(a)
    return a.sum(), xs.mean(), ys.mean(), xs.min(), xs.max(), ys.min(), ys.max()


def crop_to_alpha(arr, pad=2):
    _, _, _, x0, x1, y0, y1 = alpha_stats(arr)
    return arr[max(0, y0 - pad):y1 + pad + 1, max(0, x0 - pad):x1 + pad + 1]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sheet")
    ap.add_argument("--frames")
    ap.add_argument("--name", required=True)
    ap.add_argument("--rows", type=int, default=2)
    ap.add_argument("--cols", type=int, default=3)
    ap.add_argument("--idle-cells", default="1", help="idle 格子序号,1 基,逗号分隔(多帧 idle 给多个)")
    ap.add_argument("--idle-count", type=int, default=1, help="--frames 模式下前几张是 idle")
    ap.add_argument("--run-order", default=None, help="跑动格子序号,1 基,逗号分隔,默认 idle 之外按行序")
    ap.add_argument("--out", default="src/assets")
    args = ap.parse_args()

    if args.sheet:
        from rembg import remove, new_session
        sheet = Image.open(args.sheet).convert("RGB")
        W, H = sheet.size
        cw, ch = W // args.cols, H // args.rows
        session = new_session("u2net")
        cells = []
        for r in range(args.rows):
            for c in range(args.cols):
                cell = sheet.crop((c * cw, r * ch, (c + 1) * cw, (r + 1) * ch))
                cells.append(np.asarray(remove(cell, session=session), dtype=np.uint8))
        idle_idx = [int(x) - 1 for x in args.idle_cells.split(",")]
        idles = [cells[i] for i in idle_idx]
        rest = [i for i in range(len(cells)) if i not in idle_idx]
        order = [int(x) - 1 for x in args.run_order.split(",")] if args.run_order else rest
        runs = [cells[i] for i in order]
    else:
        paths = args.frames.split(",")
        imgs = [np.asarray(Image.open(p).convert("RGBA"), dtype=np.uint8) for p in paths]
        idles, runs = imgs[: args.idle_count], imgs[args.idle_count:]

    ni = len(idles)
    frames = [crop_to_alpha(f) for f in list(idles) + list(runs)]
    stats = [alpha_stats(f) for f in frames]
    areas = np.array([s[0] for s in stats], dtype=float)
    med = np.median(areas)
    scales = np.sqrt(med / areas)

    # 跑帧弹跳:质心 y 相对跑帧均值的偏移(用各自 scale 换算到目标尺度);idle 帧不加
    run_cy = [stats[i][2] * scales[i] for i in range(ni, len(frames))]
    mean_cy = float(np.mean(run_cy))
    offsets = [0.0] * ni + [max(-BOUNCE_MAX, min(BOUNCE_MAX, cy - mean_cy)) for cy in run_cy]

    # 防裁剪:含偏移后最大半径必须塞进 (CANVAS/2 - MARGIN),不够整体再缩
    need = 0.0
    for i, (f, s) in enumerate(zip(frames, scales)):
        _, cx, cy, x0, x1, y0, y1 = stats[i]
        half_w = max(cx - x0, x1 - cx) * s
        half_h = max(cy - y0, y1 - cy) * s + abs(offsets[i])
        need = max(need, half_w, half_h)
    fit = min(1.0, (CANVAS / 2 - MARGIN) / need)
    if fit < 1.0:
        print("整体缩放 %.3f 适配 %d 画布(高清源图的常规操作)" % (fit, CANVAS))
    scales = [s * fit for s in scales]
    offsets = [o * fit for o in offsets]

    idle_names = [f"{args.name}-idle.png"] if ni == 1 else [f"{args.name}-idle-{i}.png" for i in range(1, ni + 1)]
    names = idle_names + [f"{args.name}-run-{i}.png" for i in range(1, len(frames) - ni + 1)]
    out_area = []
    for f, s, off, nm in zip(frames, scales, offsets, names):
        h, w = f.shape[:2]
        fr = Image.fromarray(f, "RGBA").resize((max(1, round(w * s)), max(1, round(h * s))), Image.LANCZOS)
        arr = np.asarray(fr, dtype=np.uint8)
        area, cx, cy, *_ = alpha_stats(arr)
        out_area.append(area)
        canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
        canvas.paste(fr, (round(CANVAS / 2 - cx), round(CANVAS / 2 - cy + off)), fr)
        canvas.save(f"{args.out.rstrip('/')}/{nm}")
        print("%-18s area=%d offset=%+.1f" % (nm, area, off))

    disp = CANVAS * (TARGET_SCREEN_AREA / float(np.median(out_area))) ** 0.5
    print("建议显示尺寸(体量对齐旺财基准):%.1fpx 见方,CSS 一条规则全状态通用" % disp)


if __name__ == "__main__":
    main()
