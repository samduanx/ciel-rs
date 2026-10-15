"""ciel proot backend compatibility patch (guest-side).

Two fixes for the proot/Android environment:

1. Disable shutil's zero-copy fast paths (copy_file_range/sendfile).
   Defensive: these syscalls can return EINVAL mid-copy on this
   environment; the plain read/write loop is immune.

2. Replace shutil.copytree with an l2s-aware implementation.
   proot's link2symlink extension emulates hardlink() (unsupported on
   this device's fs) by storing real data in a hidden `.l2s.tmp_*`
   file and making the original name a symlink, while FAKING lstat so
   the guest sees a regular file.  But getdents64 still reports
   DT_LNK, so CPython's `_copytree` sees `entry.is_symlink() == True`
   and calls `os.readlink()` -- which proot deliberately fails with
   EINVAL (to preserve the disguise).  That killed acbs's git fetcher
   (`fetch.py:207 shutil.copytree`) on bzip3's pack files with
   `[Errno 22] Invalid argument: '.../pack-*.pack'`.
   See docs/M3-TEST-PLAN.md section 12/13.

   Our copytree treats a symlink whose readlink fails as a regular
   file: copy2() opens through the link and copies the real content.
   All other semantics mirror CPython 3.14's implementation.

This file is loaded automatically by Python's `site` module at startup
for every interpreter in the guest, so acbs-build is covered without
patching acbs itself.
"""

try:
    import os
    import shutil

    # ---- 1. no zero-copy fast paths ------------------------------
    if hasattr(shutil, "_USE_CP_COPY_FILE_RANGE"):
        shutil._USE_CP_COPY_FILE_RANGE = False
    if hasattr(shutil, "_USE_CP_SENDFILE"):
        shutil._USE_CP_SENDFILE = False

    # ---- 2. l2s-aware copytree -----------------------------------
    _copy2 = shutil.copy2
    _copystat = shutil.copystat
    _Error = shutil.Error

    def _l2s_copytree(src, dst, symlinks=False, ignore=None,
                      copy_function=None, ignore_dangling_symlinks=False,
                      dirs_exist_ok=False):
        if copy_function is None:
            copy_function = _copy2
        os.makedirs(dst, exist_ok=dirs_exist_ok)
        errors = []
        with os.scandir(src) as it:
            entries = list(it)
        if ignore is not None:
            ignored_names = ignore(os.fspath(src), [x.name for x in entries])
        else:
            ignored_names = ()

        for srcentry in entries:
            if srcentry.name in ignored_names:
                continue
            srcname = os.path.join(src, srcentry.name)
            dstname = os.path.join(dst, srcentry.name)
            try:
                is_symlink = srcentry.is_symlink()
                linkto = None
                if is_symlink:
                    try:
                        linkto = os.readlink(srcname)
                    except OSError:
                        # proot link2symlink: d_type says LNK, but
                        # readlink is deliberately EINVAL'd and lstat
                        # is faked as a regular file.  Copy through.
                        is_symlink = False
                if is_symlink:
                    if symlinks:
                        os.symlink(linkto, dstname)
                        _copystat(srcname, dstname, follow_symlinks=False)
                        continue
                    if not os.path.exists(srcname) and ignore_dangling_symlinks:
                        continue
                    if srcentry.is_dir():
                        _l2s_copytree(srcname, dstname, symlinks, ignore,
                                      copy_function, ignore_dangling_symlinks,
                                      dirs_exist_ok)
                    else:
                        copy_function(srcname, dstname)
                elif srcentry.is_dir():
                    _l2s_copytree(srcname, dstname, symlinks, ignore,
                                  copy_function, ignore_dangling_symlinks,
                                  dirs_exist_ok)
                else:
                    copy_function(srcname, dstname)
            except _Error as err:
                errors.extend(err.args[0])
            except OSError as why:
                errors.append((srcname, dstname, str(why)))
        if errors:
            raise _Error(errors)
        _copystat(src, dst, follow_symlinks=not symlinks)
        return dst

    shutil.copytree = _l2s_copytree
except Exception:
    # Never break interpreter startup over a compatibility shim.
    pass
