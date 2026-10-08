# Draws the Flask icon and writes assets\flask.ico (16-256 px) plus a 256 px PNG preview.
# The icon: a cream-outlined flask of milk whose surface is a soft usage curve.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$dir = $PSScriptRoot

function New-Color([string]$hex, [int]$a = 255) {
    $v = [Convert]::ToInt32($hex, 16)
    [System.Drawing.Color]::FromArgb($a, ($v -shr 16) -band 255, ($v -shr 8) -band 255, $v -band 255)
}
function New-Points([double[]]$xy) {
    $pts = New-Object 'System.Drawing.PointF[]' ($xy.Length / 2)
    for ($i = 0; $i -lt $pts.Length; $i++) { $pts[$i] = New-Object System.Drawing.PointF ([single]$xy[2 * $i]), ([single]$xy[2 * $i + 1]) }
    , $pts
}

function New-FlaskBitmap([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.PixelOffsetMode = 'HighQuality'
    $g.Clear([System.Drawing.Color]::Transparent)
    # Everything below is drawn on a 256 x 256 design grid.
    $g.ScaleTransform($size / 256.0, $size / 256.0)
    $small = $size -le 32

    $body = New-Object System.Drawing.Drawing2D.GraphicsPath
    $body.AddPolygon((New-Points @(104,38, 152,38, 152,100, 218,206, 200,232, 56,232, 38,206, 104,100)))

    # Glass.
    $g.FillPath((New-Object System.Drawing.SolidBrush (New-Color '34302b')), $body)

    # Milk, with a soft usage curve as its surface.
    $surface = New-Points @(20,178, 72,150, 104,174, 140,120, 176,162, 236,150)
    $liquid = New-Object System.Drawing.Drawing2D.GraphicsPath
    $liquid.AddCurve($surface, 0.45)
    $liquid.AddLine(236, 150, 236, 244)
    $liquid.AddLine(236, 244, 20, 244)
    $liquid.CloseFigure()
    $g.SetClip($body)
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush `
        (New-Object System.Drawing.PointF 0, 110), (New-Object System.Drawing.PointF 0, 236), (New-Color 'fffdf8'), (New-Color 'e6d8be')
    $g.FillPath($grad, $liquid)
    if (-not $small) {
        $froth = New-Object System.Drawing.SolidBrush (New-Color 'ffffff' 190)
        $g.FillEllipse($froth, 84, 196, 18, 18)
        $g.FillEllipse($froth, 144, 190, 12, 12)
    }
    $g.ResetClip()

    # Bubbles rising up the neck; too fine to survive at small sizes.
    if (-not $small) {
        $bubble = New-Object System.Drawing.SolidBrush (New-Color 'f3ece0')
        $g.FillEllipse($bubble, 112, 76, 16, 16)
        $g.FillEllipse($bubble, 132, 54, 11, 11)
    }

    # Outline and lip.
    $glass = New-Color 'f3ece0'
    $pen = New-Object System.Drawing.Pen $glass, $(if ($small) { 20 } else { 12 })
    $pen.LineJoin = 'Round'
    $g.DrawPath($pen, $body)
    $lipPen = New-Object System.Drawing.Pen $glass, $(if ($small) { 24 } else { 18 })
    $lipPen.StartCap = 'Round'; $lipPen.EndCap = 'Round'
    $g.DrawLine($lipPen, 96, 32, 160, 32)

    $g.Dispose()
    $bmp
}

function Get-PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    , $ms.ToArray()
}

# Classic 32-bit DIB icon image: header, bottom-up BGRA rows, then an (empty) 1-bit AND mask.
function Get-DibBytes($bmp) {
    $n = $bmp.Width
    $ms = New-Object System.IO.MemoryStream
    $w = New-Object System.IO.BinaryWriter $ms
    $w.Write([int]40); $w.Write([int]$n); $w.Write([int]($n * 2)); $w.Write([int16]1); $w.Write([int16]32)
    $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0); $w.Write([int]0)
    $rect = New-Object System.Drawing.Rectangle 0, 0, $n, $n
    $data = $bmp.LockBits($rect, 'ReadOnly', [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $row = New-Object byte[] ($n * 4)
    for ($y = $n - 1; $y -ge 0; $y--) {
        [System.Runtime.InteropServices.Marshal]::Copy([IntPtr]($data.Scan0.ToInt64() + $y * $data.Stride), $row, 0, $row.Length)
        $w.Write($row)
    }
    $bmp.UnlockBits($data)
    $maskRow = New-Object byte[] ([int]([Math]::Ceiling($n / 32.0) * 4))
    for ($y = 0; $y -lt $n; $y++) { $w.Write($maskRow) }
    , $ms.ToArray()
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 256
$images = foreach ($s in $sizes) {
    $bmp = New-FlaskBitmap $s
    if ($s -eq 256) { $bmp.Save("$dir\flask-256.png", [System.Drawing.Imaging.ImageFormat]::Png) }
    # 256 px is stored as PNG, as Windows expects; smaller sizes as DIBs for the resource compiler.
    [pscustomobject]@{ Size = $s; Bytes = $(if ($s -eq 256) { Get-PngBytes $bmp } else { Get-DibBytes $bmp }) }
}

$out = New-Object System.IO.MemoryStream
$w = New-Object System.IO.BinaryWriter $out
$w.Write([int16]0); $w.Write([int16]1); $w.Write([int16]$images.Count)
$offset = 6 + 16 * $images.Count
foreach ($img in $images) {
    $w.Write([byte]($img.Size % 256)); $w.Write([byte]($img.Size % 256)); $w.Write([byte]0); $w.Write([byte]0)
    $w.Write([int16]1); $w.Write([int16]32); $w.Write([int]$img.Bytes.Length); $w.Write([int]$offset)
    $offset += $img.Bytes.Length
}
foreach ($img in $images) { $w.Write($img.Bytes) }
[System.IO.File]::WriteAllBytes("$dir\flask.ico", $out.ToArray())
"wrote flask.ico ($($out.Length) bytes, sizes: $($sizes -join ', ')) and flask-256.png"
