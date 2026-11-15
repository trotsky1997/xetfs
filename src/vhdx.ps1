$ErrorActionPreference='Stop'
Set-StrictMode -Version Latest
$env:PSModulePath=Join-Path $PSHOME 'Modules'
$creationHandle=$null
try {
 $action=$env:XETFS_VHDX_ACTION
 $image=[IO.Path]::GetFullPath($env:XETFS_VHDX_IMAGE)
 $drive=([string]$env:XETFS_VHDX_DRIVE).ToUpperInvariant()
 if([IO.Path]::GetExtension($image) -ine '.vhdx' -or $image -match '["\r\n]' -or $image.StartsWith('\\')){throw 'Expected a local .vhdx path without quotes or line breaks'}
 $sidecar=$image+'.xetfs.json'
 if(-not $drive -and $action -ne 'create'){$saved=[IO.File]::ReadAllText($sidecar) | ConvertFrom-Json;$drive=([string]$saved.Drive).ToUpperInvariant()}
 if($drive -notmatch '^[E-Z]$'){throw 'Use one available drive letter E through Z'}
 $identity=New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
 if(-not $identity.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)){throw 'Administrator privileges are required'}
 # Serialize management without leaving a stale lock after a crash.
 $lock=[IO.File]::Open($image+'.xetfs.lock',[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
 try {
  if($action -eq 'create') {
   $gib=[long]$env:XETFS_VHDX_SIZE_GIB
   if($gib -lt 1 -or $gib -gt 16384){throw 'size-gib must be between 1 and 16384'}
   if((Test-Path -LiteralPath $image) -or (Test-Path -LiteralPath $sidecar)){throw 'Image or ownership record already exists; refusing overwrite'}
   if(Get-PSDrive -Name $drive -ErrorAction SilentlyContinue){throw 'Drive letter is already in use'}
   Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class XetCreateVirtualDisk {
 [StructLayout(LayoutKind.Sequential)] struct Storage {public uint Device;public Guid Vendor;}
 [StructLayout(LayoutKind.Sequential,CharSet=CharSet.Unicode)] struct Parameters {
  public uint Version;public uint UnionPadding;public Guid Id;public ulong Size;public uint Block;public uint Sector;public uint PhysicalSector;
  [MarshalAs(UnmanagedType.LPWStr)]public string Parent;
  [MarshalAs(UnmanagedType.LPWStr)]public string Source;
  public uint OpenFlags;public Storage ParentType;public Storage SourceType;public Guid Resiliency;
 }
 [DllImport("virtdisk.dll",CharSet=CharSet.Unicode)] static extern uint CreateVirtualDisk(ref Storage type,string path,uint access,IntPtr security,uint flags,uint providerFlags,ref Parameters parameters,IntPtr overlapped,out SafeFileHandle handle);
 public static SafeFileHandle Create(string path,ulong size) {
  var type=new Storage{Device=3,Vendor=new Guid("EC984AEC-A0F9-47E9-901F-71415A66345B")};
  var p=new Parameters{Version=2,Id=Guid.NewGuid(),Size=size,Block=2097152,Sector=512,PhysicalSector=4096};
  SafeFileHandle h;uint e=CreateVirtualDisk(ref type,path,0,IntPtr.Zero,0,0,ref p,IntPtr.Zero,out h);
  if(e!=0){if(h!=null)h.Dispose();throw new System.ComponentModel.Win32Exception((int)e,"CreateVirtualDisk failed");}return h;
 }
}
'@
   $creationHandle=[XetCreateVirtualDisk]::Create($image,[uint64]($gib*1GB))
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
   $partition | Set-Partition -NewDriveLetter $drive | Out-Null
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
     $partition | Set-Partition -NewDriveLetter $drive | Out-Null
     $record.Drive=$drive
     $record | ConvertTo-Json | Set-Content -LiteralPath $sidecar -Encoding UTF8
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
 public static SafeFileHandle Lock(string p) {
  var h=CreateFileW(p,0xC0000000,3,IntPtr.Zero,3,0,IntPtr.Zero);
  try {
   if(h.IsInvalid)throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   uint r;if(!DeviceIoControl(h,0x90018,IntPtr.Zero,0,IntPtr.Zero,0,out r,IntPtr.Zero))throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error(),"Volume is busy; stop repository users before detaching");
   if(!FlushFileBuffers(h))throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
   return h;
  }catch{h.Dispose();throw;}
 }
}
'@
     if(-not $partition.DriveLetter){throw 'Volume has no drive letter; inspect manually'}
     $volumeLock=[XetVolumeLock]::Lock('\\.\'+$partition.DriveLetter+':')
     try{Dismount-DiskImage -ImagePath $image | Out-Null}finally{$volumeLock.Dispose()}
    }
   }
  }
  $v=Get-DiskImage -ImagePath $image
  $letter=$null
  if($v.Attached){$letter=[string](Get-Partition -DiskNumber ($v | Get-Disk).Number | Where-Object {[string]$_.Guid -eq $record.PartitionGuid}).DriveLetter}
  [pscustomobject]@{Image=$image;Dynamic=$true;LogicalBytes=$record.SizeBytes;ImageFileBytes=(Get-Item -LiteralPath $image).Length;Attached=$v.Attached;Drive=$letter;RepositoryPath=if($letter){$letter+':\repository'}else{$null};OwnershipRecord=$sidecar} | ConvertTo-Json
 } finally {if($creationHandle){$creationHandle.Dispose()};$lock.Dispose()}
} catch {Write-Error $_;exit 1}
