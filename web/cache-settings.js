// Shared browser/worker cache policy. Only the preference is persisted.
(() => {
    const MIB = 1024 * 1024;
    const DEFAULT_BYTES = 1024 * MIB;
    const KEY = 'binwave-file-cache-mib';
    function fromMiB(value) {
        const mib = Number(value);
        const bytes = mib * MIB;
        if (String(value).trim() === '' || !Number.isSafeInteger(mib) || mib < 0 || !Number.isSafeInteger(bytes)) {
            throw Error('Enter a whole number of MiB, 0 or greater.');
        }
        return bytes;
    }
    function load(storage) {
        try {
            const saved = storage?.getItem(KEY);
            if (saved !== null && saved !== undefined) return fromMiB(saved);
        } catch { /* Unavailable storage or invalid saved preference. */ }
        return DEFAULT_BYTES;
    }
    globalThis.binwaveCacheSettings = {MIB, DEFAULT_BYTES, KEY, fromMiB, load};
})();
