"""生成窗屿（WindowIsle）的图标资源：assets/windowisle-logo.png（1024px）与
assets/windowisle.ico（16/24/32/48/64/128/256）。

设计（草稿 C「极简胶囊」）：靛蓝到天青的斜向渐变圆角方块，屏幕轮廓的淡白描边，
顶部一颗黑色胶囊岛，岛里一个绿色圆点和一道白色短条，岛后有一圈柔光。
16/24/32 像素单独画简化版：去掉描边和柔光、胶囊加大，托盘里也认得出。

用法：python scripts/make_logo.py（需要 Pillow）
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter

S = 2048  # 超采样画布边长
ASSETS = Path(__file__).resolve().parent.parent / "assets"

INDIGO = (79, 70, 229)
SKY = (56, 189, 248)
ISLAND = (10, 10, 14)
DOT = (52, 211, 153)


def squircle_mask(size, radius=0.225, pad=0.02):
    mask = Image.new("L", (size, size), 0)
    p = int(size * pad)
    ImageDraw.Draw(mask).rounded_rectangle([p, p, size - p, size - p], radius=int(size * radius), fill=255)
    return mask


def diagonal_gradient(size, start, end):
    # 沿左上到右下的对角线插值；先在小图上算再放大，避免逐像素循环太慢。
    small = 256
    ramp = Image.new("L", (small, small))
    ramp.putdata([int(255 * (x + y) / (2 * (small - 1))) for y in range(small) for x in range(small)])
    ramp = ramp.resize((size, size), Image.BICUBIC)
    return Image.composite(Image.new("RGB", (size, size), end), Image.new("RGB", (size, size), start), ramp)


def on_layer(img, draw):
    layer = Image.new("RGBA", img.size, (0, 0, 0, 0))
    draw(ImageDraw.Draw(layer))
    img.alpha_composite(layer)


def pill(draw, cx, cy, w, h, fill):
    draw.rounded_rectangle([cx - w / 2, cy - h / 2, cx + w / 2, cy + h / 2], radius=h / 2, fill=fill)


def clip(img):
    img.putalpha(ImageChops.multiply(img.getchannel("A"), squircle_mask(img.width)))
    return img


def full():
    """大尺寸：完整设计。"""
    img = diagonal_gradient(S, INDIGO, SKY).convert("RGBA")
    on_layer(img, lambda d: d.rounded_rectangle([S * 0.14, S * 0.14, S * 0.86, S * 0.86], radius=S * 0.16,
                                                outline=(255, 255, 255, 90), width=int(S * 0.018)))
    glow = Image.new("RGBA", img.size, (0, 0, 0, 0))
    r = S * 0.20
    ImageDraw.Draw(glow).ellipse([S * 0.5 - r, S * 0.30 - r, S * 0.5 + r, S * 0.30 + r], fill=(255, 255, 255, 90))
    img.alpha_composite(glow.filter(ImageFilter.GaussianBlur(80)))
    d = ImageDraw.Draw(img)
    pill(d, S * 0.5, S * 0.30, S * 0.46, S * 0.15, ISLAND)
    rd = S * 0.026
    d.ellipse([S * 0.36 - rd, S * 0.30 - rd, S * 0.36 + rd, S * 0.30 + rd], fill=DOT)
    pill(d, S * 0.55, S * 0.30, S * 0.15, S * 0.03, (255, 255, 255, 235))
    return clip(img)


def compact():
    """16–32 像素：去掉描边和柔光，胶囊放大并略下移到视觉中心偏上。"""
    img = diagonal_gradient(S, INDIGO, SKY).convert("RGBA")
    d = ImageDraw.Draw(img)
    cy = S * 0.42
    pill(d, S * 0.5, cy, S * 0.70, S * 0.26, ISLAND)
    rd = S * 0.055
    d.ellipse([S * 0.29 - rd, cy - rd, S * 0.29 + rd, cy + rd], fill=DOT)
    pill(d, S * 0.58, cy, S * 0.22, S * 0.06, (255, 255, 255))
    return clip(img)


def main():
    big, small = full(), compact()
    big.resize((1024, 1024), Image.LANCZOS).save(ASSETS / "windowisle-logo.png")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    frames = [(small if s <= 32 else big).resize((s, s), Image.LANCZOS) for s in sizes]
    # Pillow 按 sizes 从最大的源图缩放；每个尺寸单独给出，保证小尺寸用简化版。
    frames[-1].save(ASSETS / "windowisle.ico", format="ICO", sizes=[(s, s) for s in sizes], append_images=frames[:-1])
    print("written", ASSETS / "windowisle-logo.png", ASSETS / "windowisle.ico")


if __name__ == "__main__":
    main()
