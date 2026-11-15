param([string]$Binary=(Join-Path $PSScriptRoot '..\target\release\xetfs.exe'))
$ErrorActionPreference='Stop'
$Binary=(Resolve-Path $Binary).Path
$root=Join-Path $env:TEMP ('xetfs-vhdx-test-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
$image=Join-Path $root 'test.vhdx'
function Cli([string[]]$a){$out=& $Binary @a;if($LASTEXITCODE -ne 0){throw ('CLI failed: '+($a -join ' '))};$out}
try {
 $first=Cli @('vhdx','create',$image,'--size-gib','2','--drive','V') | ConvertFrom-Json
 if(-not $first.Attached -or $first.LogicalBytes -ne 2GB -or $first.ImageFileBytes -ge 512MB){throw 'Dynamic allocation verification failed'}
 $before=(Get-FileHash ($image+'.xetfs.json')).Hash
 $ErrorActionPreference='Continue'
 & $Binary vhdx create $image --size-gib 2 --drive V 2>$null | Out-Null
 $ErrorActionPreference='Stop'
 if($LASTEXITCODE -eq 0 -or (Get-FileHash ($image+'.xetfs.json')).Hash -ne $before){throw 'Overwrite protection failed'}
 Cli @('--repo','V:\repository','init') | Out-Null
 $sample=Join-Path $root 'sample.bin'
 $data=New-Object byte[] (1MB);[Random]::new(512).NextBytes($data);[IO.File]::WriteAllBytes($sample,$data)
 Cli @('--repo','V:\repository','put',$sample,'/sample.bin') | Out-Null
 Cli @('vhdx','detach',$image) | Out-Null
 Cli @('vhdx','attach',$image,'--drive','V') | Out-Null
 $export=Join-Path $root 'export.bin'
 Cli @('--repo','V:\repository','get','/sample.bin',$export) | Out-Null
 if((Get-FileHash $sample).Hash -ne (Get-FileHash $export).Hash){throw 'Reattach integrity failed'}
 Cli @('--repo','V:\repository','fsck') | Out-Null
 $handle=[IO.File]::Open('V:\repository\held-open.test',[IO.FileMode]::Create,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
 try{$ErrorActionPreference='Continue';& $Binary vhdx detach $image 2>$null | Out-Null;$ErrorActionPreference='Stop';if($LASTEXITCODE -eq 0){throw 'Busy volume detach unexpectedly succeeded'}}finally{$ErrorActionPreference='Stop';$handle.Dispose()}
 $last=Cli @('vhdx','status',$image) | ConvertFrom-Json
 if(-not $last.Attached){throw 'Busy volume was detached'}
 Cli @('vhdx','detach',$image) | Out-Null
 [pscustomobject]@{Passed=$true;DynamicAllocation=$true;OverwriteRejected=$true;ReattachHash=$true;BusyDetachRejected=$true;InitialImageBytes=$first.ImageFileBytes;LogicalBytes=$first.LogicalBytes} | ConvertTo-Json
} finally {
 if((Get-DiskImage -ImagePath $image -ErrorAction SilentlyContinue).Attached){Dismount-DiskImage -ImagePath $image}
 # Only this script's freshly created fixture directory.
 if([IO.Path]::GetFullPath($root).StartsWith([IO.Path]::GetFullPath($env:TEMP)+'\') -and (Split-Path $root -Leaf) -match '^xetfs-vhdx-test-[0-9a-f]{32}$'){Remove-Item -LiteralPath $root -Recurse -Force}
}
