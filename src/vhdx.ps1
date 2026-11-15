$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
try {
 $action=$env:XETFS_VHDX_ACTION
 $image=[IO.Path]::GetFullPath($env:XETFS_VHDX_IMAGE)
 $drive=$env:XETFS_VHDX_DRIVE.ToUpperInvariant()
 if([IO.Path]::GetExtension($image) -ine '.vhdx' -or $image -match '["\r\n]' -or $image.StartsWith('\\')){throw 'Expected a local .vhdx path without quotes or line breaks'}
 if($drive -notmatch '^[E-Z]$'){throw 'Use one available drive letter E through Z'}
 $identity=New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
 if(-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'Administrator privileges are required'}
 $sidecar=$image+'.xetfs.json'
 # Serialize management without leaving a stale lock after a crash.
 $lock=[IO.File]::Open($image+'.xetfs.lock',[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
 try {
  if($action -eq 'create') {
   $gib=[long]$env:XETFS_VHDX_SIZE_GIB
   if($gib -lt 1 -or $gib -gt 16384){throw 'size-gib must be between 1 and 16384'}
   if((Test-Path -LiteralPath $image) -or (Test-Path -LiteralPath $sidecar)){throw 'Image or ownership record already exists; refusing overwrite'}
   if(Get-PSDrive -Name $drive -ErrorAction SilentlyContinue){throw 'Drive letter is already in use'}
   $script=[IO.Path]::GetTempFileName()
   try {
    [IO.File]::WriteAllText($script,('create vdisk file="'+$image+'" maximum='+($gib*1024)+' type=expandable'+[Environment]::NewLine+'exit'),[Text.Encoding]::Unicode)
    $output=& diskpart.exe /s $script 2>&1
    if($LASTEXITCODE -ne 0 -or -not(Test-Path -LiteralPath $image)){throw ('VHDX creation failed: '+($output -join ' '))}
   } finally {Remove-Item -LiteralPath $script -Force}
   $record=[ordered]@{Schema=1;Image=$image;SizeBytes=$gib*1GB;DiskUniqueId=$null;PartitionGuid=$null;Drive=$drive;CreatedUtc=[DateTime]::UtcNow.ToString('o');State='created'}
   $record | ConvertTo-Json | Set-Content -LiteralPath $sidecar -Encoding UTF8
   $v=Mount-DiskImage -ImagePath $image -NoDriveLetter -PassThru
   $disk=$v | Get-Disk
   if($disk.IsBoot -or $disk.IsSystem -or $disk.PartitionStyle -ne 'RAW' -or $disk.Size -ne $record.SizeBytes -or [string]$disk.BusType -ne 'File Backed Virtual'){throw 'New disk identity/size/type check failed; refusing format'}
   $record.DiskUniqueId=$disk.UniqueId
   $record.State='initializing'
   $record | ConvertTo-Json | Set-Content -LiteralPath $sidecar -Encoding UTF8
   $disk | Initialize-Disk -PartitionStyle GPT -PassThru | Out-Null
   $partition=New-Partition -DiskNumber $disk.Number -UseMaximumSize
   $partition | Format-Volume -FileSystem NTFS -NewFileSystemLabel XetFS-Backing -Confirm:$false | Out-Null
   $record.PartitionGuid=[string]$partition.Guid
   $record.State='ready'
   $record | ConvertTo-Json | Set-Content -LiteralPath $sidecar -Encoding UTF8
   $partition | Set-Partition -NewDriveLetter $drive
  } else {
   if(-not(Test-Path -LiteralPath $sidecar)){throw 'Missing XetFS ownership record; refusing management'}
   $record=[IO.File]::ReadAllText($sidecar) | ConvertFrom-Json
   if($record.Schema -ne 1 -or $record.Image -ine $image -or $record.State -ne 'ready'){throw 'Ownership record mismatch or incomplete creation; inspect manually'}
   $v=Get-DiskImage -ImagePath $image
   if($action -eq 'attach' -and -not $v.Attached){$v=Mount-DiskImage -ImagePath $image -NoDriveLetter -PassThru}
   if($v.Attached){
    $disk=$v | Get-Disk
    if($disk.IsBoot -or $disk.IsSystem -or $disk.UniqueId -ne $record.DiskUniqueId -or $disk.Size -ne $record.SizeBytes){throw 'Disk identity changed; refusing operation'}
    $partition=Get-Partition -DiskNumber $disk.Number | Where-Object {[string]$_.Guid -eq $record.PartitionGuid}
    if(@($partition).Count -ne 1){throw 'Owned partition not found'}
    if($action -eq 'attach' -and [string]$partition.DriveLetter -ne $drive){
     if(Get-PSDrive -Name $drive -ErrorAction SilentlyContinue){throw 'Drive letter is already in use'}
     $partition | Set-Partition -NewDriveLetter $drive
    }
    if($action -eq 'detach'){
     # Lock and flush the filesystem first; an active repository/mount must close its handles.
     Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class XetVolumeLock {
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)] static extern SafeFileHandle CreateFileW(string p,uint a,uint s,IntPtr z,uint c,uint f,IntPtr t);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool DeviceIoControl(SafeFileHandle h,uint c,IntPtr i,uint n,IntPtr o,uint m,out uint r,IntPtr v);
 [DllImport("kernel32.dll",SetLastError=true)] static extern bool FlushFileBuffers(SafeFileHandle h);
 public static void Check(string p) {
  using(var h=CreateFileW(p,0xC0000000,3,IntPtr.Zero,3,0,IntPtr.Zero)) {
   if(h.IsInvalid)throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   uint r;if(!DeviceIoControl(h,0x90018,IntPtr.Zero,0,IntPtr.Zero,0,out r,IntPtr.Zero))throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(),"Volume is busy; stop repository users before detaching");
   if(!FlushFileBuffers(h))throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
  }
 }
}
'@
     if(-not $partition.DriveLetter){throw 'Volume has no drive letter; inspect manually'}
     [XetVolumeLock]::Check('\\.\'+$partition.DriveLetter+':')
     Dismount-DiskImage -ImagePath $image
    }
   }
  }
  $v=Get-DiskImage -ImagePath $image
  $letter=$null
  if($v.Attached){$letter=[string](Get-Partition -DiskNumber ($v | Get-Disk).Number | Where-Object {[string]$_.Guid -eq $record.PartitionGuid}).DriveLetter}
  [pscustomobject]@{Image=$image;Dynamic=$true;LogicalBytes=$record.SizeBytes;ImageFileBytes=(Get-Item -LiteralPath $image).Length;Attached=$v.Attached;Drive=$letter;RepositoryPath=if($letter){$letter+':\repository'}else{$null};OwnershipRecord=$sidecar} | ConvertTo-Json
 } finally {$lock.Dispose()}
} catch {Write-Error $_;exit 1}
