# Replaces all icons in a program file with the icons from an .ico file.
# build.ps1 uses it for the setup .exe, because IExpress always uses its own icon.
param([Parameter(Mandatory)][string]$Exe, [Parameter(Mandatory)][string]$Ico)
$ErrorActionPreference = 'Stop'
Add-Type @'
using System; using System.Collections.Generic; using System.IO; using System.Runtime.InteropServices;
public static class IconRes {
    delegate bool NameCb(IntPtr m, IntPtr type, IntPtr name, IntPtr l);
    delegate bool LangCb(IntPtr m, IntPtr type, IntPtr name, ushort lang, IntPtr l);
    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern IntPtr LoadLibraryEx(string f, IntPtr h, uint flags);
    [DllImport("kernel32.dll")] static extern bool FreeLibrary(IntPtr m);
    [DllImport("kernel32.dll")] static extern bool EnumResourceNames(IntPtr m, IntPtr type, NameCb cb, IntPtr l);
    [DllImport("kernel32.dll")] static extern bool EnumResourceLanguages(IntPtr m, IntPtr type, IntPtr name, LangCb cb, IntPtr l);
    [DllImport("kernel32.dll", SetLastError=true, CharSet=CharSet.Unicode)] static extern IntPtr BeginUpdateResource(string f, bool deleteAll);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool UpdateResource(IntPtr h, IntPtr type, IntPtr name, ushort lang, byte[] data, int size);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool EndUpdateResource(IntPtr h, bool discard);

    // A resource name is a small integer id or a pointer to a string. A string is copied, because the pointer is valid only in the callback.
    static IntPtr Keep(IntPtr name) { return ((long)name >> 16) == 0 ? name : Marshal.StringToHGlobalUni(Marshal.PtrToStringUni(name)); }

    static List<Tuple<IntPtr, IntPtr, ushort>> Find(string exe) {
        var found = new List<Tuple<IntPtr, IntPtr, ushort>>();
        IntPtr m = LoadLibraryEx(exe, IntPtr.Zero, 2); // LOAD_LIBRARY_AS_DATAFILE
        if (m == IntPtr.Zero) throw new IOException("cannot open " + exe);
        foreach (int t in new[] { 14, 3 }) { // RT_GROUP_ICON, RT_ICON
            IntPtr type = (IntPtr)t;
            EnumResourceNames(m, type, (mod, ty, name, l) => {
                IntPtr kept = Keep(name);
                EnumResourceLanguages(mod, ty, name, (mo, tt, nn, lang, ll) => { found.Add(Tuple.Create(type, kept, lang)); return true; }, IntPtr.Zero);
                return true;
            }, IntPtr.Zero);
        }
        FreeLibrary(m);
        return found;
    }

    public static int Set(string exe, string ico) {
        var old = Find(exe);
        byte[] f = File.ReadAllBytes(ico);
        int count = BitConverter.ToUInt16(f, 4);
        IntPtr h = BeginUpdateResource(exe, false);
        if (h == IntPtr.Zero) throw new IOException("cannot change " + exe);
        foreach (var r in old) UpdateResource(h, r.Item1, r.Item2, r.Item3, null, 0);
        // The .ico header is 6 bytes, then 16 bytes for each image. The group resource has 14 bytes for each image: the image offset becomes its resource id.
        var group = new byte[6 + 14 * count];
        Array.Copy(f, group, 6);
        for (int i = 0; i < count; i++) {
            int e = 6 + 16 * i;
            int size = BitConverter.ToInt32(f, e + 8), offset = BitConverter.ToInt32(f, e + 12);
            var image = new byte[size];
            Array.Copy(f, offset, image, 0, size);
            if (!UpdateResource(h, (IntPtr)3, (IntPtr)(i + 1), 0, image, size)) throw new IOException("cannot add icon image " + (i + 1));
            Array.Copy(f, e, group, 6 + 14 * i, 12);
            BitConverter.GetBytes((ushort)(i + 1)).CopyTo(group, 6 + 14 * i + 12);
        }
        if (!UpdateResource(h, (IntPtr)14, (IntPtr)1, 0, group, group.Length)) throw new IOException("cannot add the icon group");
        if (!EndUpdateResource(h, false)) throw new IOException("cannot save " + exe);
        return old.Count;
    }
}
'@
$removed = [IconRes]::Set((Resolve-Path $Exe).Path, (Resolve-Path $Ico).Path)
Write-Host "Icon set: removed $removed old icon resources, added the images from $Ico."
