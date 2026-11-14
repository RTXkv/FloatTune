"""生成 FloatTune 图标：主图标 + 托盘小图标"""
from PIL import Image, ImageDraw
import os

S = 512
img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

# 圆角矩形蒙版
mask = Image.new("L", (S, S), 0)
md = ImageDraw.Draw(mask)
md.rounded_rectangle([16, 16, S - 16, S - 16], radius=110, fill=255)

# 深蓝→紫渐变背景
grad = Image.new("RGBA", (S, S))
gd = ImageDraw.Draw(grad)
for y in range(S):
    t = y / S
    r = int(28 + 46 * t)
    g = int(22 + 16 * t)
    b = int(58 + 96 * t)
    gd.line([(0, y), (S, y)], fill=(r, g, b, 255))
img.paste(grad, (0, 0), mask)

d = ImageDraw.Draw(img)
white = (255, 255, 255, 238)
soft = (255, 255, 255, 130)

# 音符：符头 + 符杆 + 符尾
d.ellipse([150, 310, 268, 428], fill=white)
d.rectangle([248, 168, 272, 380], fill=white)
d.polygon([(272, 168), (382, 196), (382, 252), (272, 224)], fill=white)

# “浮”的波浪线
d.arc([96, 236, 416, 420], start=205, end=335, fill=soft, width=20)

out_dir = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "icons")
os.makedirs(out_dir, exist_ok=True)
img.save(os.path.join(out_dir, "app-icon.png"))
img.resize((32, 32), Image.LANCZOS).save(os.path.join(out_dir, "tray.png"))
img.resize((256, 256), Image.LANCZOS).save(os.path.join(out_dir, "128x128.png"))
print("icons generated")
