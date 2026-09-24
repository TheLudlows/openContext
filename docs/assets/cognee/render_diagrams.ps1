# Windows raster renderer; no external modules required.
# Run: powershell -NoProfile -File docs/assets/cognee/render_diagrams.ps1
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$fontName = 'Microsoft YaHei UI'
function Brush([string]$hex) { return [System.Drawing.SolidBrush]::new([System.Drawing.ColorTranslator]::FromHtml($hex)) }
function Text($g, [string]$value, [single]$x, [single]$baseline, [single]$size, [bool]$bold=$false, [string]$color='#20334b') {
    $style = [System.Drawing.FontStyle]::Regular
    if ($bold) { $style = [System.Drawing.FontStyle]::Bold }
    $f = [System.Drawing.Font]::new($fontName,$size,$style,[System.Drawing.GraphicsUnit]::Pixel)
    $b = Brush $color
    $g.DrawString($value,$f,$b,$x,($baseline-$size-2))
    $f.Dispose(); $b.Dispose()
}
Get-ChildItem -LiteralPath $PSScriptRoot -Filter '*.json' | Where-Object Name -Match '^\d\d_' | ForEach-Object {
    $d = Get-Content -LiteralPath $_.FullName -Raw -Encoding UTF8 | ConvertFrom-Json
    $bmp = [System.Drawing.Bitmap]::new([int]$d.width,[int]$d.height)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.Clear([System.Drawing.Color]::White)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
    Text $g $d.title 65 67 38 $true
    Text $g $d.subtitle 65 115 25 $false '#5d7087'
    foreach ($a in $d.arrows) {
        $pen = [System.Drawing.Pen]::new([System.Drawing.ColorTranslator]::FromHtml('#66788f'),3)
        $cap = [System.Drawing.Drawing2D.AdjustableArrowCap]::new(5,6)
        $pen.CustomEndCap = $cap
        $points = [System.Drawing.PointF[]]@($a.points | ForEach-Object { [System.Drawing.PointF]::new([single]$_[0],[single]$_[1]) })
        $g.DrawLines($pen,$points)
        if ($a.label) { Text $g $a.label $a.lx $a.ly 22 }
        $pen.Dispose(); $cap.Dispose()
    }
    foreach ($b in $d.boxes) {
        $fill = Brush $b.fill
        $border = [System.Drawing.Pen]::new([System.Drawing.ColorTranslator]::FromHtml('#c7d4e2'),1)
        $g.FillRectangle($fill,[single]$b.x,[single]$b.y,[single]$b.w,[single]$b.h)
        $g.DrawRectangle($border,[single]$b.x,[single]$b.y,[single]$b.w,[single]$b.h)
        Text $g $b.title ($b.x+22) ($b.y+43) 29 $true
        for ($i=0; $i -lt $b.lines.Count; $i++) { Text $g $b.lines[$i] ($b.x+22) ($b.y+83+33*$i) 24 }
        $fill.Dispose(); $border.Dispose()
    }
    foreach ($n in $d.notes) { Text $g $n.text $n.x $n.y 24 $false '#5d7087' }
    Text $g $d.footer 65 ($d.height-25) 20 $false '#718198'
    $dest = Join-Path $PSScriptRoot ($d.name+'.png')
    $bmp.Save($dest,[System.Drawing.Imaging.ImageFormat]::Png)
    $g.Dispose(); $bmp.Dispose()
    Write-Output $dest
}
