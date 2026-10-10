$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$outDir = Split-Path $PSScriptRoot -Parent
$base = @(Get-Content (Join-Path $PSScriptRoot 'approved-grid.txt'))
$format = [System.Drawing.StringFormat]::GenericTypographic.Clone()
$format.FormatFlags = $format.FormatFlags -bor [System.Drawing.StringFormatFlags]::MeasureTrailingSpaces
function Get-Color([int]$x,[int]$y) {
  if ((($x-20)/4.2)*(($x-20)/4.2)+(($y-3)/2.8)*(($y-3)/2.8) -le 1) { return '#F68D32' }
  $left = 5 + [Math]::Abs($x-8)*0.95
  if ($x -gt 8 -and $x -lt 17 -and $y -lt $left+3) { return '#BEAD8A' }
  return '#FFF5D5'
}
function Render($rows,[int]$size,[int]$brandX,[int]$brandY) {
  $scale = $size / 256
  $bitmap = [System.Drawing.Bitmap]::new($size,$size,[System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $graphics.Clear([System.Drawing.ColorTranslator]::FromHtml('#262722'))
  $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
  $font = [System.Drawing.Font]::new('Consolas',[single](13*$scale),[System.Drawing.FontStyle]::Bold,[System.Drawing.GraphicsUnit]::Pixel)
  if ($font.Name -ne 'Consolas') { throw 'Consolas is required.' }
  for ($y=0;$y -lt 18;$y++) {
    for ($x=0;$x -lt 28;$x++) {
      if ($rows[$y][$x] -eq ' ') { continue }
      $color = Get-Color $x $y
      if ($y -eq $brandY -and $x -ge $brandX -and $x -lt $brandX+10) { $color = '#FFFFFF' }
      $brush = [System.Drawing.SolidBrush]::new([System.Drawing.ColorTranslator]::FromHtml($color))
      $graphics.DrawString([string]$rows[$y][$x],$font,$brush,[single]((16+8*$x)*$scale),[single]((20+12*$y)*$scale),$format)
      $brush.Dispose()
    }
  }
  $font.Dispose(); $graphics.Dispose()
  return $bitmap
}
# Reproduce the original first, and require pixel equality if the reference is present.
$reference = Join-Path (Split-Path $outDir -Parent) 'approved-W1.png'
if (Test-Path $reference) {
  $original = [System.Drawing.Bitmap]::new($reference)
  $baseline = Render $base 256 -1 -1
  $different = 0
  for ($y=0;$y -lt 256;$y++) { for ($x=0;$x -lt 256;$x++) {
    if ($original.GetPixel($x,$y).ToArgb() -ne $baseline.GetPixel($x,$y).ToArgb()) { $different++ }
  } }
  $original.Dispose(); $baseline.Dispose()
  if ($different) { throw "Baseline differs from approval by $different pixels." }
  Write-Output 'Approved baseline: pixel-identical.'
}
$variants = @(
  @{Name='P1'; X=6; Y=17; Old='SUN RISES OVER'; New='CLIPSCRIBE SKY'},
  @{Name='P2'; X=2; Y=11; Old='RISES OVER'; New='CLIPSCRIBE'},
  @{Name='P3'; X=0; Y=13; Old='PEAKS SNOW'; New='CLIPSCRIBE'}
)
foreach ($v in $variants) {
  $rows = $base.Clone()
  if ($rows[$v.Y].Substring($v.X,$v.Old.Length) -cne $v.Old) { throw 'Unexpected source text' }
  $rows[$v.Y] = $rows[$v.Y].Remove($v.X,$v.Old.Length).Insert($v.X,$v.New)
  if (($rows | Where-Object { $_.Length -ne 28 }).Count) { throw 'Grid width changed' }
  if ([regex]::Matches(($rows -join "`n"),'CLIPSCRIBE').Count -ne 1) { throw 'Brand count must equal one' }
  $dir = Join-Path $outDir $v.Name
  New-Item -ItemType Directory -Force $dir | Out-Null
  $rows | Set-Content (Join-Path $PSScriptRoot "$($v.Name)-grid.txt")
  $native = Render $rows 256 $v.X $v.Y
  $native.Save((Join-Path $dir 'clipscribe-256.png'),[System.Drawing.Imaging.ImageFormat]::Png)
  $native.Dispose()
  $large = Render $rows 512 $v.X $v.Y
  $large.Save((Join-Path $dir 'clipscribe-512.png'),[System.Drawing.Imaging.ImageFormat]::Png)
  foreach ($size in @(128,64,32)) {
    $thumb = [System.Drawing.Bitmap]::new($size,$size,[System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($thumb)
    $g.CompositingQuality = [System.Drawing.Drawing2D.CompositingQuality]::HighQuality
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $attrs = [System.Drawing.Imaging.ImageAttributes]::new()
    $attrs.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY)
    $g.DrawImage($large,[System.Drawing.Rectangle]::new(0,0,$size,$size),0,0,512,512,[System.Drawing.GraphicsUnit]::Pixel,$attrs)
    $thumb.Save((Join-Path $dir "clipscribe-$size.png"),[System.Drawing.Imaging.ImageFormat]::Png)
    $attrs.Dispose(); $g.Dispose(); $thumb.Dispose()
  }
  $large.Dispose()
  Write-Output "$($v.Name): five sizes rendered."
}
$format.Dispose()
