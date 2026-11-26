const begExecute = (() => {
  const output = text;
  const stringify = JSON.stringify;
  const decode = Uint8Array.fromBase64.bind(Uint8Array);
  const read = tools.beg_filter_read;
  const write = tools.beg_filter_write;
  const env = tools.beg_filter_env;

  Object.defineProperty(Math, "random", { value: undefined });
  for (const name of [
    "Date", "Temporal", "Intl", "performance", "crypto", "WeakRef", "FinalizationRegistry",
    "setTimeout", "clearTimeout", "tools", "ALL_TOOLS", "store", "load", "notify",
    "yield_control", "exit", "text", "content", "image", "audio", "generatedImage", "console",
  ]) {
    Object.defineProperty(globalThis, name, { value: undefined });
  }

  const io = Object.freeze({
    async env(name) {
      if (name === undefined) return env({});
      if (typeof name === "string") return env({ name });
      if (name instanceof Uint8Array) return env({ base64: name.toBase64() });
      throw new TypeError("io.env expects a variable name as a string or Uint8Array");
    },
    async read(maxBytes = 16384) {
      return decode(await read({ max_bytes: maxBytes }));
    },
    async write(stream, data) {
      if (typeof data === "string") {
        for (let start = 0; start < data.length;) {
          let end = Math.min(data.length, start + 4096);
          const last = data.charCodeAt(end - 1);
          if (end < data.length && last >= 0xd800 && last <= 0xdbff) end -= 1;
          await write({ stream, text: data.slice(start, end) });
          start = end;
        }
      } else if (data instanceof Uint8Array) {
        for (let start = 0; start < data.length; start += 16384) {
          await write({ stream, base64: data.subarray(start, start + 16384).toBase64() });
        }
      } else {
        throw new TypeError("io.write expects a string or Uint8Array");
      }
    },
  });

  return async (program, args, phase) => {
    if (typeof program.checkArgs !== "function" || typeof program.run !== "function") {
      throw new TypeError("the filter must declare checkArgs and run");
    }
    const argv = args.map((arg) => decode(arg));
    if (phase === "check") {
      output(stringify(await program.checkArgs(argv, io)));
    } else if (phase === "run") {
      output(stringify(await program.run(argv, io)));
    } else {
      output("null");
    }
  };
})();
