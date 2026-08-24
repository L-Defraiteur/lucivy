// This code implements the `-sMODULARIZE` settings by taking the generated
// JS program code (INNER_JS_CODE) and wrapping it in a factory function.

// When targeting node and ES6 we use `await import ..` in the generated code
// so the outer function needs to be marked as async.
async function createLucivy(moduleArg = {}) {
  var Module = moduleArg;
// include: shell.js
// include: minimum_runtime_check.js
(function() {
  // "30.0.0" -> 300000
  function humanReadableVersionToPacked(str) {
    str = str.split("-")[0];
    // Remove any trailing part from e.g. "12.53.3-alpha"
    var vers = str.split(".").slice(0, 3);
    while (vers.length < 3) vers.push("00");
    vers = vers.map((n, i, arr) => n.padStart(2, "0"));
    return vers.join("");
  }
  // 300000 -> "30.0.0"
  var packedVersionToHumanReadable = n => [ n / 1e4 | 0, (n / 100 | 0) % 100, n % 100 ].join(".");
  var TARGET_NOT_SUPPORTED = 2147483647;
  // Note: We use a typeof check here instead of optional chaining using
  // globalThis because older browsers might not have globalThis defined.
  // We skip the node version checking when running on Bun/Deno since the node
  // version they report doesn't seem to be useful.
  if (typeof process !== "undefined" && !process.versions?.bun && typeof Deno == "undefined") {
    var currentNodeVersion = process.versions?.node ? humanReadableVersionToPacked(process.versions.node) : TARGET_NOT_SUPPORTED;
    if (currentNodeVersion < 180300) {
      throw new Error(`This emscripten-generated code requires node v${packedVersionToHumanReadable(180300)} (detected v${packedVersionToHumanReadable(currentNodeVersion)})`);
    }
  }
  var userAgent = typeof navigator !== "undefined" && navigator.userAgent;
  if (!userAgent) {
    return;
  }
  var currentSafariVersion = userAgent.includes("Safari/") && !userAgent.includes("Chrome/") && userAgent.match(/Version\/(\d+\.?\d*\.?\d*)/) ? humanReadableVersionToPacked(userAgent.match(/Version\/(\d+\.?\d*\.?\d*)/)[1]) : TARGET_NOT_SUPPORTED;
  if (currentSafariVersion < 15e4) {
    throw new Error(`This emscripten-generated code requires Safari v${packedVersionToHumanReadable(15e4)} (detected v${currentSafariVersion})`);
  }
  var currentFirefoxVersion = userAgent.match(/Firefox\/(\d+(?:\.\d+)?)/) ? parseFloat(userAgent.match(/Firefox\/(\d+(?:\.\d+)?)/)[1]) : TARGET_NOT_SUPPORTED;
  if (currentFirefoxVersion < 114) {
    throw new Error(`This emscripten-generated code requires Firefox v114 (detected v${currentFirefoxVersion})`);
  }
  var currentChromeVersion = userAgent.match(/Chrome\/(\d+(?:\.\d+)?)/) ? parseFloat(userAgent.match(/Chrome\/(\d+(?:\.\d+)?)/)[1]) : TARGET_NOT_SUPPORTED;
  if (currentChromeVersion < 85) {
    throw new Error(`This emscripten-generated code requires Chrome v85 (detected v${currentChromeVersion})`);
  }
})();

// end include: minimum_runtime_check.js
// The Module object: Our interface to the outside world. We import
// and export values on it. There are various ways Module can be used:
// 1. Not defined. We create it here
// 2. A function parameter, function(moduleArg) => Promise<Module>
// 3. pre-run appended it, var Module = {}; ..generated code..
// 4. External script tag defines var Module.
// We need to check if Module already exists (e.g. case 3 above).
// Substitution will be replaced with actual code on later stage of the build,
// this way Closure Compiler will not mangle it (e.g. case 4. above).
// Note that if you want to run closure, and also to use Module
// after the generated code, you will need to define   var Module = {};
// before the code. Then that object will be used in the code, and you
// can continue to use Module afterwards as well.
// Determine the runtime environment we are in. You can customize this by
// setting the ENVIRONMENT setting at compile time (see settings.js).
// Attempt to auto-detect the environment
var ENVIRONMENT_IS_WEB = !!globalThis.window;

var ENVIRONMENT_IS_WORKER = !!globalThis.WorkerGlobalScope;

// N.b. Electron.js environment is simultaneously a NODE-environment, but
// also a web environment.
var ENVIRONMENT_IS_NODE = globalThis.process?.versions?.node && globalThis.process?.type != "renderer";

var ENVIRONMENT_IS_SHELL = !ENVIRONMENT_IS_WEB && !ENVIRONMENT_IS_NODE && !ENVIRONMENT_IS_WORKER;

// Three configurations we can be running in:
// 1) We could be the application main() thread running in the main JS UI thread. (ENVIRONMENT_IS_WORKER == false and ENVIRONMENT_IS_PTHREAD == false)
// 2) We could be the application main() running directly in a worker. (ENVIRONMENT_IS_WORKER == true, ENVIRONMENT_IS_PTHREAD == false)
// 3) We could be an application pthread running in a worker. (ENVIRONMENT_IS_WORKER == true and ENVIRONMENT_IS_PTHREAD == true)
// The way we signal to a worker that it is hosting a pthread is to construct
// it with a specific name.
var ENVIRONMENT_IS_PTHREAD = ENVIRONMENT_IS_WORKER && globalThis.name?.startsWith("em-pthread");

if (ENVIRONMENT_IS_PTHREAD) {
  assert(!globalThis.moduleLoaded, "module should only be loaded once on each pthread worker");
  globalThis.moduleLoaded = true;
}

if (ENVIRONMENT_IS_NODE) {
  // When building an ES module `require` is not normally available.
  // We need to use `createRequire()` to construct the require()` function.
  const {createRequire} = await import("node:module");
  /** @suppress{duplicate} */ var require = createRequire(import.meta.url);
  var worker_threads = require("node:worker_threads");
  globalThis.Worker = worker_threads.Worker;
  ENVIRONMENT_IS_WORKER = !worker_threads.isMainThread;
  // Under node we set `workerData` to `em-pthread` to signal that the worker
  // is hosting a pthread.
  ENVIRONMENT_IS_PTHREAD = ENVIRONMENT_IS_WORKER && worker_threads.workerData == "em-pthread";
}

// --pre-jses are emitted after the Module integration code, so that they can
// refer to Module (if they choose; they can also define Module)
var programArgs = [];

var thisProgram = "./this.program";

var quit_ = (status, toThrow) => {
  throw toThrow;
};

var _scriptName = import.meta.url;

// `/` should be present at the end if `scriptDirectory` is not empty
var scriptDirectory = "";

function locateFile(path) {
  if (Module["locateFile"]) {
    return Module["locateFile"](path, scriptDirectory);
  }
  return scriptDirectory + path;
}

// Hooks that are implemented differently in different runtime environments.
var readAsync, readBinary;

if (ENVIRONMENT_IS_NODE) {
  const isNode = globalThis.process?.versions?.node && globalThis.process?.type != "renderer";
  if (!isNode) throw new Error("not compiled for this environment (did you build to HTML and try to run it not on the web, or set ENVIRONMENT to something - like node - and run it someplace else - like on the web?)");
  // These modules will usually be used on Node.js. Load them eagerly to avoid
  // the complexity of lazy-loading.
  var fs = require("node:fs");
  if (_scriptName.startsWith("file:")) {
    scriptDirectory = require("node:path").dirname(require("node:url").fileURLToPath(_scriptName)) + "/";
  }
  // include: node_shell_read.js
  readBinary = filename => {
    // We need to re-wrap `file://` strings to URLs.
    filename = isFileURI(filename) ? new URL(filename) : filename;
    var ret = fs.readFileSync(filename);
    assert(Buffer.isBuffer(ret));
    return ret;
  };
  readAsync = async (filename, binary = true) => {
    // See the comment in the `readBinary` function.
    filename = isFileURI(filename) ? new URL(filename) : filename;
    var ret = fs.readFileSync(filename, binary ? undefined : "utf8");
    assert(binary ? Buffer.isBuffer(ret) : typeof ret == "string");
    return ret;
  };
  // end include: node_shell_read.js
  if (process.argv.length > 1) {
    thisProgram = process.argv[1].replace(/\\/g, "/");
  }
  programArgs = process.argv.slice(2);
  quit_ = (status, toThrow) => {
    process.exitCode = status;
    throw toThrow;
  };
} else if (ENVIRONMENT_IS_SHELL) {} else // Note that this includes Node.js workers when relevant (pthreads is enabled).
// Node.js workers are detected as a combination of ENVIRONMENT_IS_WORKER and
// ENVIRONMENT_IS_NODE.
if (ENVIRONMENT_IS_WEB || ENVIRONMENT_IS_WORKER) {
  try {
    scriptDirectory = new URL(".", _scriptName).href;
  } catch {}
  if (!(globalThis.window || globalThis.WorkerGlobalScope)) throw new Error("not compiled for this environment (did you build to HTML and try to run it not on the web, or set ENVIRONMENT to something - like node - and run it someplace else - like on the web?)");
  // Differentiate the Web Worker from the Node Worker case, as reading must
  // be done differently.
  if (!ENVIRONMENT_IS_NODE) {
    // include: web_or_worker_shell_read.js
    if (ENVIRONMENT_IS_WORKER) {
      readBinary = url => {
        var xhr = new XMLHttpRequest;
        xhr.open("GET", url, false);
        xhr.responseType = "arraybuffer";
        xhr.send(null);
        return new Uint8Array(/** @type{!ArrayBuffer} */ (xhr.response));
      };
    }
    readAsync = async url => {
      // Fetch has some additional restrictions over XHR, like it can't be used on a file:// url.
      // See https://github.com/github/fetch/pull/92#issuecomment-140665932
      // Cordova or Electron apps are typically loaded from a file:// url.
      // So use XHR on webview if URL is a file URL.
      if (isFileURI(url)) {
        return new Promise((resolve, reject) => {
          var xhr = new XMLHttpRequest;
          xhr.open("GET", url, true);
          xhr.responseType = "arraybuffer";
          xhr.onload = () => {
            if (xhr.status == 200 || (xhr.status == 0 && xhr.response)) {
              // file URLs can return 0
              resolve(xhr.response);
              return;
            }
            reject(xhr.status);
          };
          xhr.onerror = reject;
          xhr.send(null);
        });
      }
      var response = await fetch(url, {
        credentials: "same-origin"
      });
      if (response.ok) {
        return response.arrayBuffer();
      }
      throw new Error(response.status + " : " + response.url);
    };
  }
} else {
  throw new Error("environment detection error");
}

// Set up the out() and err() hooks, which are how we can print to stdout or
// stderr, respectively.
// Normally just binding console.log/console.error here works fine, but
// under node (with workers) we see missing/out-of-order messages so route
// directly to stdout and stderr.
// See https://github.com/emscripten-core/emscripten/issues/14804
var defaultPrint = console.log.bind(console);

var defaultPrintErr = console.error.bind(console);

if (ENVIRONMENT_IS_NODE) {
  var utils = require("node:util");
  var stringify = a => typeof a == "object" ? utils.inspect(a) : a;
  defaultPrint = (...args) => fs.writeSync(1, args.map(stringify).join(" ") + "\n");
  defaultPrintErr = (...args) => fs.writeSync(2, args.map(stringify).join(" ") + "\n");
}

var out = defaultPrint;

var err = defaultPrintErr;

// perform assertions in shell.js after we set up out() and err(), as otherwise
// if an assertion fails it cannot print the message
assert(ENVIRONMENT_IS_WEB || ENVIRONMENT_IS_WORKER || ENVIRONMENT_IS_NODE, "pthreads do not work in this environment yet (need Web Workers, or an alternative to them)");

assert(!ENVIRONMENT_IS_SHELL, "shell environment detected but not enabled at build time (add `shell` to `-sENVIRONMENT` to enable)");

// end include: shell.js
// include: preamble.js
// === Preamble library stuff ===
// Documentation for the public APIs defined in this file must be updated in:
//    site/source/docs/api_reference/preamble.js.rst
// A prebuilt local version of the documentation is available at:
//    site/build/text/docs/api_reference/preamble.js.txt
// You can also build docs locally as HTML or other formats in site/
// An online HTML version (which may be of a different version of Emscripten)
//    is up at http://kripken.github.io/emscripten-site/docs/api_reference/preamble.js.html
var wasmBinary;

if (!globalThis.WebAssembly) {
  err("no native wasm support detected");
}

// Wasm globals
// For sending to workers.
var wasmModule;

//========================================
// Runtime essentials
//========================================
// whether we are quitting the application. no code should run after this.
// set in exit() and abort()
var ABORT = false;

// set by exit() and abort().  Passed to 'onExit' handler.
// NOTE: This is also used as the process return code in shell environments
// but only when noExitRuntime is false.
var EXITSTATUS;

// In STRICT mode, we only define assert() when ASSERTIONS is set.  i.e. we
// don't define it at all in release modes.  This matches the behaviour of
// MINIMAL_RUNTIME.
// TODO(sbc): Make this the default even without STRICT enabled.
/** @type {function(*, string=)} */ function assert(condition, text) {
  if (!condition) {
    abort("Assertion failed" + (text ? ": " + text : ""));
  }
}

// We used to include malloc/free by default in the past. Show a helpful error in
// builds with assertions.
/**
 * Indicates whether filename is delivered via file protocol (as opposed to http/https)
 * @noinline
 */ var isFileURI = filename => filename.startsWith("file://");

// include: runtime_common.js
// include: runtime_exceptions.js
// Base Emscripten EH error class
class EmscriptenEH extends Error {}

class EmscriptenSjLj extends EmscriptenEH {}

class CppException extends EmscriptenEH {
  constructor(excPtr) {
    super(excPtr);
    this.excPtr = excPtr;
    const excInfo = getExceptionMessage(this);
    this.name = excInfo[0];
    this.message = excInfo[1];
  }
}

// end include: runtime_exceptions.js
// include: runtime_debug.js
var runtimeDebug = true;

// Switch to false at runtime to disable logging at the right times
// Used by XXXXX_DEBUG settings to output debug messages.
function dbg(...args) {
  if (!runtimeDebug && typeof runtimeDebug != "undefined") return;
  // Avoid using the console for debugging in multi-threaded node applications
  // See https://github.com/emscripten-core/emscripten/issues/14804
  if (ENVIRONMENT_IS_NODE) {
    // TODO(sbc): Unify with err/out implementation in shell.sh.
    var fs = require("node:fs");
    var utils = require("node:util");
    function stringify(a) {
      switch (typeof a) {
       case "object":
        return utils.inspect(a);

       case "undefined":
        return "undefined";
      }
      return a;
    }
    fs.writeSync(2, args.map(stringify).join(" ") + "\n");
  } else // TODO(sbc): Make this configurable somehow.  Its not always convenient for
  // logging to show up as warnings.
  console.warn(...args);
}

// Endianness check
(() => {
  var h16 = new Int16Array(1);
  var h8 = new Int8Array(h16.buffer);
  h16[0] = 25459;
  if (h8[0] !== 115 || h8[1] !== 99) abort("Runtime error: expected the system to be little-endian! (Run with -sSUPPORT_BIG_ENDIAN to bypass)");
})();

function consumedModuleProp(prop) {
  var value = Module[prop];
  var msg = `Attempt to modify \`Module.${prop}\` after it has already been processed.  This can happen, for example, when code is injected via '--post-js' rather than '--pre-js'`;
  if (Array.isArray(value)) {
    value = new Proxy(value, {
      set(target, key, val) {
        abort(msg);
        return false;
      },
      defineProperty(target, key, descriptor) {
        abort(msg);
        return false;
      },
      deleteProperty(target, key) {
        abort(msg);
        return false;
      }
    });
  }
  Object.defineProperty(Module, prop, {
    configurable: true,
    get() {
      return value;
    },
    set() {
      abort(msg);
    }
  });
}

function makeInvalidEarlyAccess(name) {
  return () => assert(false, `call to '${name}' via reference taken before Wasm module initialization`);
}

function ignoredModuleProp(prop) {
  if (Object.getOwnPropertyDescriptor(Module, prop)) {
    abort(`\`Module.${prop}\` was supplied but \`${prop}\` not included in INCOMING_MODULE_JS_API`);
  }
}

// forcing the filesystem exports a few things by default
function isExportedByForceFilesystem(name) {
  return name === "FS_createPath" || name === "FS_createDataFile" || name === "FS_createPreloadedFile" || name === "FS_preloadFile" || name === "FS_unlink" || name === "addRunDependency" || name === "removeRunDependency";
}

function missingLibrarySymbol(sym) {
  // Any symbol that is not included from the JS library is also (by definition)
  // not exported on the Module object.
  unexportedRuntimeSymbol(sym);
}

function unexportedRuntimeSymbol(sym) {
  if (ENVIRONMENT_IS_PTHREAD) {
    return;
  }
  if (!Object.getOwnPropertyDescriptor(Module, sym)) {
    Object.defineProperty(Module, sym, {
      configurable: true,
      get() {
        var msg = `'${sym}' was not exported. add it to EXPORTED_RUNTIME_METHODS (see the Emscripten FAQ)`;
        if (isExportedByForceFilesystem(sym)) {
          msg += ". Alternatively, forcing filesystem support (-sFORCE_FILESYSTEM) can export this for you";
        }
        abort(msg);
      }
    });
  }
}

/**
 * Override `err`/`out`/`dbg` to report thread / worker information
 */ function initWorkerLogging() {
  function getLogPrefix() {
    var t = 0;
    if (runtimeInitialized && typeof _pthread_self != "undefined") {
      t = _pthread_self();
    }
    return `w:${workerID},t:${ptrToString(t)}:`;
  }
  // Prefix all dbg() messages with the calling thread info.
  var origDbg = dbg;
  dbg = (...args) => origDbg(getLogPrefix(), ...args);
}

initWorkerLogging();

// end include: runtime_debug.js
// include: runtime_stack_check.js
const stackCookie1 = 34821223;

const stackCookie2 = 2310721022;

// Initializes the stack cookie. Called at the startup of main and at the startup of each thread in pthreads mode.
function writeStackCookie() {
  var max = _emscripten_stack_get_end();
  assert((max & 3) == 0);
  // If the stack ends at address zero we write our cookies 4 bytes into the
  // stack.  This prevents interference with SAFE_HEAP and ASAN which also
  // monitor writes to address zero.
  if (max == 0) {
    max += 4;
  }
  // The stack grow downwards towards _emscripten_stack_get_end.
  // We write cookies to the final two words in the stack and detect if they are
  // ever overwritten.
  (growMemViews(), HEAPU32)[((max) >>> 2) >>> 0] = stackCookie1;
  (growMemViews(), HEAPU32)[(((max) + (4)) >>> 2) >>> 0] = stackCookie2;
  // Also test the global address 0 for integrity.
  (growMemViews(), HEAPU32)[((0) >>> 2) >>> 0] = 1668509029;
}

function u32ToHexString(num) {
  return "0x" + (num >>> 0).toString(16).padStart(8, "0");
}

function checkStackCookie() {
  if (ABORT) return;
  var max = _emscripten_stack_get_end();
  // See writeStackCookie().
  if (max == 0) {
    max += 4;
  }
  var val1 = (growMemViews(), HEAPU32)[((max) >>> 2) >>> 0];
  var val2 = (growMemViews(), HEAPU32)[(((max) + (4)) >>> 2) >>> 0];
  if (val1 != stackCookie1 || val2 != stackCookie2) {
    abort(`Stack overflow! Stack cookie has been overwritten at ${ptrToString(max)}, expected hex dwords ${u32ToHexString(stackCookie2)} and ${u32ToHexString(stackCookie1)}, but received ${u32ToHexString(val2)} ${u32ToHexString(val1)}`);
  }
  // Also test the global address 0 for integrity.
  if ((growMemViews(), HEAPU32)[((0) >>> 2) >>> 0] != 1668509029) {
    abort("Runtime error: The application has corrupted its heap memory area (address zero)!");
  }
}

// end include: runtime_stack_check.js
// Support for growable heap + pthreads, where the buffer may change, so JS views
// must be updated.
function growMemViews() {
  // `updateMemoryViews` updates all the views simultaneously, so it's enough to check any of them.
  if (wasmMemory.buffer != HEAP8.buffer) {
    updateMemoryViews();
  }
}

if (ENVIRONMENT_IS_NODE && (ENVIRONMENT_IS_PTHREAD)) {
  // Create as web-worker-like an environment as we can.
  globalThis.self = globalThis;
  var parentPort = worker_threads.parentPort;
  // Deno and Bun already have `postMessage` defined on the global scope and
  // deliver messages to `globalThis.onmessage`, so we must not duplicate that
  // behavior here if `postMessage` is already present.
  if (!globalThis.postMessage) {
    parentPort.on("message", msg => globalThis.onmessage?.({
      data: msg
    }));
    globalThis.postMessage = msg => parentPort.postMessage(msg);
  }
  // Node.js Workers do not pass postMessage()s and uncaught exception events to the parent
  // thread necessarily in the same order where they were generated in sequential program order.
  // See https://github.com/nodejs/node/issues/59617
  // To remedy this, capture all uncaughtExceptions in the Worker, and sequentialize those over
  // to the same postMessage pipe that other messages use.
  process.on("uncaughtException", err => {
    postMessage({
      cmd: 8,
      error: err
    });
    // Also shut down the Worker to match the same semantics as if this uncaughtException
    // handler was not registered.
    // (n.b. this will not shut down the whole Node.js app process, but just the Worker)
    process.exit(1);
  });
}

// include: runtime_pthread.js
// Pthread Web Worker handling code.
// This code runs only on pthread web workers and handles pthread setup
// and communication with the main thread via postMessage.
// Unique ID of the current pthread worker (zero on non-pthread-workers
// including the main thread).
var workerID = 0;

var startWorker;

if (ENVIRONMENT_IS_PTHREAD) {
  // Thread-local guard variable for one-time init of the JS state
  var initializedJS = false;
  // Turn unhandled rejected promises into errors so that the main thread will be
  // notified about them.
  self.onunhandledrejection = e => {
    throw e.reason || e;
  };
  function handleMessage(e) {
    try {
      var msgData = e.data;
      //dbg('msgData: ' + Object.keys(msgData));
      var cmd = msgData.cmd;
      if (cmd == 1) {
        // Preload command that is called once per worker to parse and load the Emscripten code.
        workerID = msgData.workerID;
        // Until we initialize the runtime, queue up any further incoming messages.
        let messageQueue = [];
        self.onmessage = e => messageQueue.push(e);
        // And add a callback for when the runtime is initialized.
        startWorker = () => {
          // Notify the main thread that this thread has loaded.
          postMessage({
            cmd: 3
          });
          // Process any messages that were queued before the thread was ready.
          for (let msg of messageQueue) {
            handleMessage(msg);
          }
          // Restore the real message handler.
          self.onmessage = handleMessage;
        };
        // Use `const` here to ensure that the variable is scoped only to
        // that iteration, allowing safe reference from a closure.
        for (const handler of msgData.handlers) {
          // If the main module has a handler for a certain event, but no
          // handler exists on the pthread worker, then proxy that handler
          // back to the main thread.
          if (!Module[handler] || Module[handler].proxy) {
            Module[handler] = (...args) => {
              postMessage({
                cmd: 9,
                handler,
                args
              });
            };
            // Rebind the out / err handlers if needed
            if (handler == "print") out = Module[handler];
            if (handler == "printErr") err = Module[handler];
          }
        }
        wasmMemory = msgData.wasmMemory;
        updateMemoryViews();
        wasmModule = msgData.wasmModule;
        createWasm();
        run();
        startWorker();
      } else if (cmd == 2) {
        assert(msgData.pthread_ptr);
        assert(wasmMemory, "CMD_RUN received before CMD_LOAD");
        // Call inside JS module to set up the stack frame for this pthread in JS module scope.
        // This needs to be the first thing that we do, as we cannot call to any C/C++ functions
        // until the thread stack is initialized.
        establishStackSpace(msgData.pthread_ptr);
        // Pass the thread address to wasm to store it for fast access.
        __emscripten_thread_init(msgData.pthread_ptr, /*is_main=*/ 0, /*is_runtime=*/ 0, /*can_block=*/ 1, 0, 0);
        PThread.threadInitTLS();
        // Await mailbox notifications with `Atomics.waitAsync` so we can start
        // using the fast `Atomics.notify` notification path.
        __emscripten_thread_mailbox_await(msgData.pthread_ptr);
        if (!initializedJS) {
          initializedJS = true;
        }
        try {
          invokeEntryPoint(msgData.start_routine, msgData.arg);
        } catch (ex) {
          if (ex != "unwind") {
            // The pthread "crashed".  Do not call `_emscripten_thread_exit` (which
            // would make this thread joinable).  Instead, re-throw the exception
            // and let the top level handler propagate it back to the main thread.
            throw ex;
          }
        }
      } else if (cmd == 4) {
        if (initializedJS) {
          checkMailbox();
        }
      } else if (cmd) {
        // The received message looks like something that should be handled by this message
        // handler, (since there is a cmd field present), but is not one of the
        // recognized commands:
        err(`worker: received unknown command ${cmd}`);
        err(msgData);
      }
    } catch (ex) {
      err(`worker: onmessage() captured an uncaught exception: ${ex}`);
      if (ex?.stack) err(ex.stack);
      if (runtimeInitialized) __emscripten_thread_crashed();
      throw ex;
    }
  }
  self.onmessage = handleMessage;
}

// ENVIRONMENT_IS_PTHREAD
// end include: runtime_pthread.js
// Memory management
var runtimeInitialized = false;

// When ALLOW_MEMORY_GROWTH is enabled, the conversion from Wasm
// memory to ArrayBuffer requires some additional logic.
function getMemoryBuffer() {
  return wasmMemory.buffer;
}

function updateMemoryViews() {
  // If we already have a heap that is resizeable/growable buffer we don't
  // need to do anything in updateMemoryViews.
  if (HEAP8?.buffer?.growable) return;
  var b = getMemoryBuffer();
  HEAP8 = new Int8Array(b);
  HEAP16 = new Int16Array(b);
  Module["HEAPU8"] = HEAPU8 = new Uint8Array(b);
  HEAP32 = new Int32Array(b);
  HEAPU32 = new Uint32Array(b);
  HEAPF32 = new Float32Array(b);
  HEAPF64 = new Float64Array(b);
  HEAP64 = new BigInt64Array(b);
}

// In non-standalone/normal mode, we create the memory here.
// include: runtime_init_memory.js
// Create the wasm memory. (Note: this only applies if IMPORTED_MEMORY is defined)
// check for full engine support (use string 'subarray' to avoid closure compiler confusion)
function initMemory() {
  if ((ENVIRONMENT_IS_PTHREAD)) {
    return;
  }
  {
    var INITIAL_MEMORY = 16777216;
    assert(INITIAL_MEMORY >= 2097152, `INITIAL_MEMORY should be larger than STACK_SIZE, was ${INITIAL_MEMORY}! (STACK_SIZE=2097152)`);
    /** @suppress {checkTypes} */ wasmMemory = new WebAssembly.Memory({
      "initial": INITIAL_MEMORY / 65536,
      // In theory we should not need to emit the maximum if we want "unlimited"
      // or 4GB of memory, but VMs error on that atm, see
      // https://github.com/emscripten-core/emscripten/issues/14130
      // And in the pthreads case we definitely need to emit a maximum. So
      // always emit one.
      "maximum": 65536,
      "shared": true
    });
  }
  updateMemoryViews();
}

// end include: runtime_init_memory.js
// include: memoryprofiler.js
// end include: memoryprofiler.js
// end include: runtime_common.js
assert(globalThis.Int32Array && globalThis.Float64Array && Int32Array.prototype.subarray && Int32Array.prototype.set, "JS engine does not provide full typed array support");

function preRun() {
  assert(!ENVIRONMENT_IS_PTHREAD);
  // PThreads reuse the runtime from the main thread.
  var preRun = Module["preRun"];
  if (preRun) {
    if (typeof preRun == "function") preRun = [ preRun ];
    onPreRuns.push(...preRun);
  }
  consumedModuleProp("preRun");
  // Begin ATPRERUNS hooks
  callRuntimeCallbacks(onPreRuns);
}

function initRuntime() {
  assert(!runtimeInitialized);
  runtimeInitialized = true;
  if (ENVIRONMENT_IS_PTHREAD) return;
  setStackLimits();
  checkStackCookie();
  // No ATINITS hooks
  wasmExports["__wasm_call_ctors"]();
  // No ATPOSTCTORS hooks
  checkStackCookie();
}

function postRun() {
  checkStackCookie();
  var postRun = Module["postRun"];
  if (postRun) {
    if (typeof postRun == "function") postRun = [ postRun ];
    onPostRuns.push(...postRun);
  }
  consumedModuleProp("postRun");
  // Begin ATPOSTRUNS hooks
  callRuntimeCallbacks(onPostRuns);
}

/**
 * @param {string|number=} what
 */ function abort(what) {
  Module["onAbort"]?.(what);
  what = `Aborted(${what})`;
  // TODO(sbc): Should we remove printing and leave it up to whoever
  // catches the exception?
  err(what);
  ABORT = true;
  if (what.search(/RuntimeError: [Uu]nreachable/) >= 0) {
    what += '. "unreachable" may be due to ASYNCIFY_STACK_SIZE not being large enough (try increasing it)';
  }
  // Use a wasm runtime error, because a JS error might be seen as a foreign
  // exception, which means we'd run destructors on it. We need the error to
  // simply make the program stop.
  // FIXME This approach does not work in Wasm EH because it currently does not assume
  // all RuntimeErrors are from traps; it decides whether a RuntimeError is from
  // a trap or not based on a hidden field within the object. So at the moment
  // we don't have a way of throwing a wasm trap from JS. TODO Make a JS API that
  // allows this in the wasm spec.
  // Suppress closure compiler warning here. Closure compiler's builtin extern
  // definition for WebAssembly.RuntimeError claims it takes no arguments even
  // though it can.
  // TODO(https://github.com/google/closure-compiler/pull/3913): Remove if/when upstream closure gets fixed.
  /** @suppress {checkTypes} */ var e = new WebAssembly.RuntimeError(what);
  // Throw the error whether or not MODULARIZE is set because abort is used
  // in code paths apart from instantiation where an exception is expected
  // to be thrown when abort is called.
  throw e;
}

// show errors on likely calls to FS when it was not included
function fsMissing() {
  abort("Filesystem support (FS) was not included. The problem is that you are using files from JS, but files were not used from C/C++, so filesystem support was not auto-included. You can force-include filesystem support with -sFORCE_FILESYSTEM");
}

var FS = {
  init: fsMissing,
  createDataFile: fsMissing,
  createPreloadedFile: fsMissing,
  createLazyFile: fsMissing,
  open: fsMissing,
  mkdev: fsMissing,
  registerDevice: fsMissing,
  analyzePath: fsMissing,
  ErrnoError: fsMissing
};

function createExportWrapper(name, func, nargs) {
  assert(func);
  return (...args) => {
    assert(runtimeInitialized, `native function \`${name}\` called before runtime initialization`);
    // Only assert for too many arguments. Too few can be valid since the missing arguments will be zero filled.
    assert(args.length <= nargs, `native function \`${name}\` called with ${args.length} args but expects ${nargs}`);
    return func(...args);
  };
}

var wasmBinaryFile;

function findWasmBinary() {
  if (Module["locateFile"]) {
    return locateFile("lucivy.wasm");
  }
  // Use bundler-friendly `new URL(..., import.meta.url)` pattern; works in browsers too.
  return new URL("lucivy.wasm", import.meta.url).href;
}

function getBinarySync(file) {
  if (readBinary) {
    return readBinary(file);
  }
  // Throwing a plain string here, even though it not normally advisable since
  // this gets turning into an `abort` in instantiateArrayBuffer.
  throw "both async and sync fetching of the wasm failed";
}

async function getWasmBinary(binaryFile) {
  // If we don't have the binary yet, load it asynchronously using readAsync.
  if (!wasmBinary) {
    // Fetch the binary using readAsync
    try {
      var response = await readAsync(binaryFile);
      return new Uint8Array(response);
    } catch {}
  }
  // Otherwise, getBinarySync should be able to get it synchronously
  return getBinarySync(binaryFile);
}

async function instantiateArrayBuffer(binaryFile, imports) {
  try {
    var binary = await getWasmBinary(binaryFile);
    var instance = await WebAssembly.instantiate(binary, imports);
    return instance;
  } catch (reason) {
    err(`failed to asynchronously prepare wasm: ${reason}`);
    // Warn on some common problems.
    if (isFileURI(binaryFile)) {
      err(`warning: Loading from a file URI (${binaryFile}) is not supported in most browsers. See https://emscripten.org/docs/getting_started/FAQ.html#how-do-i-run-a-local-webserver-for-testing-why-does-my-program-stall-in-downloading-or-preparing`);
    }
    abort(reason);
  }
}

async function instantiateAsync(binary, binaryFile, imports) {
  if (!binary && !isFileURI(binaryFile) && !ENVIRONMENT_IS_NODE) {
    try {
      var response = fetch(binaryFile, {
        credentials: "same-origin"
      });
      var instantiationResult = await WebAssembly.instantiateStreaming(response, imports);
      return instantiationResult;
    } catch (reason) {
      // We expect the most common failure cause to be a bad MIME type for the binary,
      // in which case falling back to ArrayBuffer instantiation should work.
      err(`wasm streaming compile failed: ${reason}`);
      err("falling back to ArrayBuffer instantiation");
    }
  }
  return instantiateArrayBuffer(binaryFile, imports);
}

function getWasmImports() {
  assignWasmImports();
  // instrumenting imports is used in asyncify in two ways: to add assertions
  // that check for proper import use, and for JSPI we use them to set up
  // the Promise API on the import side.
  // In pthreads builds getWasmImports is called more than once but we only
  // and the instrument the imports once.
  if (!wasmImports.__instrumented) {
    wasmImports.__instrumented = true;
    Asyncify.instrumentWasmImports(wasmImports);
  }
  // prepare imports
  var imports = {
    "env": wasmImports,
    "wasi_snapshot_preview1": wasmImports
  };
  return imports;
}

// Create the wasm instance.
// Receives the wasm imports, returns the exports.
async function createWasm() {
  // Load the wasm module and create an instance of using native support in the JS engine.
  // handle a generated wasm instance, receiving its exports and
  // performing other necessary setup
  function receiveInstance(instance, module) {
    wasmExports = instance.exports;
    wasmExports = Asyncify.instrumentWasmExports(wasmExports);
    wasmExports = applySignatureConversions(wasmExports);
    registerTLSInit(wasmExports["_emscripten_tls_init"]);
    assignWasmExports(wasmExports);
    // We now have the Wasm module loaded up, keep a reference to the compiled module so we can post it to the workers.
    wasmModule = module;
    return wasmExports;
  }
  // Prefer streaming instantiation if available.
  // Async compilation can be confusing when an error on the page overwrites Module
  // (for example, if the order of elements is wrong, and the one defining Module is
  // later), so we save Module and check it later.
  var trueModule = Module;
  function receiveInstantiationResult(result) {
    // 'result' is a ResultObject object which has both the module and instance.
    // receiveInstance() will swap in the exports (to Module.asm) so they can be called
    assert(Module === trueModule, "the Module object should not be replaced during async compilation - perhaps the order of HTML elements is wrong?");
    trueModule = null;
    return receiveInstance(result["instance"], result["module"]);
  }
  var info = getWasmImports();
  // User shell pages can write their own Module.instantiateWasm = function(imports, successCallback) callback
  // to manually instantiate the Wasm module themselves. This allows pages to
  // run the instantiation parallel to any other async startup actions they are
  // performing.
  // Also pthreads and wasm workers initialize the wasm instance through this
  // path.
  var instantiateWasm = Module["instantiateWasm"];
  if (instantiateWasm) {
    return new Promise(resolve => {
      try {
        instantiateWasm(info, (inst, mod) => resolve(receiveInstance(inst, mod)));
      } catch (e) {
        err(`Module.instantiateWasm callback failed with error: ${e}`);
        throw e;
      }
    });
  }
  if ((ENVIRONMENT_IS_PTHREAD)) {
    // Instantiate from the module that was received via postMessage from
    // the main thread. We can just use sync instantiation in the worker.
    assert(wasmModule, "wasmModule should have been received via postMessage");
    var instance = new WebAssembly.Instance(wasmModule, getWasmImports());
    return receiveInstance(instance, wasmModule);
  }
  wasmBinaryFile ??= findWasmBinary();
  var result = await instantiateAsync(wasmBinary, wasmBinaryFile, info);
  var exports = receiveInstantiationResult(result);
  return exports;
}

// end include: preamble.js
// Begin JS library code
class ExitStatus {
  name="ExitStatus";
  constructor(status) {
    this.message = `Program terminated with exit(${status})`;
    this.status = status;
  }
}

/** @type {!Int32Array} */ var HEAP32;

/** @type {!Int8Array} */ var HEAP8;

/** @type {!Uint32Array} */ var HEAPU32;

var terminateWorker = worker => {
  worker.terminate();
  // terminate() can be asynchronous, so in theory the worker can continue
  // to run for some amount of time after termination.  However from our POV
  // the worker is now dead and we don't want to hear from it again, so we stub
  // out its message handler here.  This avoids having to check in each of
  // the onmessage handlers if the message was coming from a valid worker.
  worker.onmessage = e => {
    var cmd = e.data.cmd;
    err(`received "${cmd}" command from terminated worker: ${worker.workerID}`);
  };
};

var cleanupThread = pthread_ptr => {
  assert(!ENVIRONMENT_IS_PTHREAD, "cleanupThread() should only be called from the main thread");
  assert(pthread_ptr, "null pthread_ptr passed to cleanupThread");
  var worker = PThread.pthreads[pthread_ptr];
  assert(worker);
  PThread.returnWorkerToPool(worker);
};

var callRuntimeCallbacks = callbacks => {
  while (callbacks.length > 0) {
    // Pass the module as the first argument.
    callbacks.shift()(Module);
  }
};

var onPreRuns = [];

var addOnPreRun = cb => onPreRuns.push(cb);

var dependenciesPromise = null;

var resolveRunDependencies = async () => dependenciesPromise;

var runDependencies = 0;

var dependenciesPromiseResolve = null;

var runDependencyTracking = {};

var runDependencyWatcher = null;

var removeRunDependency = id => {
  runDependencies--;
  Module["monitorRunDependencies"]?.(runDependencies);
  assert(id, "removeRunDependency requires an ID");
  assert(runDependencyTracking[id]);
  delete runDependencyTracking[id];
  if (!runDependencies) {
    if (runDependencyWatcher !== null) {
      clearInterval(runDependencyWatcher);
      runDependencyWatcher = null;
    }
    dependenciesPromiseResolve();
  }
};

var addRunDependency = id => {
  if (!runDependencies) {
    dependenciesPromise = new Promise(resolve => dependenciesPromiseResolve = resolve);
  }
  runDependencies++;
  Module["monitorRunDependencies"]?.(runDependencies);
  assert(id, "addRunDependency requires an ID");
  assert(!runDependencyTracking[id]);
  runDependencyTracking[id] = 1;
  if (!runDependencyWatcher && globalThis.setInterval) {
    // Check for missing dependencies every few seconds
    runDependencyWatcher = setInterval(() => {
      if (ABORT) {
        clearInterval(runDependencyWatcher);
        runDependencyWatcher = null;
        return;
      }
      var shown = false;
      for (var dep in runDependencyTracking) {
        if (!shown) {
          shown = true;
          err("still waiting on run dependencies:");
        }
        err(`dependency: ${dep}`);
      }
      if (shown) {
        err("(end of list)");
      }
    }, 1e4);
    // Prevent this timer from keeping the runtime alive if nothing
    // else is.
    runDependencyWatcher.unref?.();
  }
};

var spawnThread = threadParams => {
  assert(!ENVIRONMENT_IS_PTHREAD, "spawnThread() should only be called from the main thread");
  assert(threadParams.pthread_ptr, "spawnThread called with null pthread ptr");
  var worker = PThread.getNewWorker();
  if (!worker) {
    // No available workers in the PThread pool.
    return 6;
  }
  assert(!worker.pthread_ptr);
  // Add to pthreads map
  PThread.pthreads[threadParams.pthread_ptr] = worker;
  worker.pthread_ptr = threadParams.pthread_ptr;
  var msg = {
    cmd: 2,
    start_routine: threadParams.startRoutine,
    arg: threadParams.arg,
    pthread_ptr: threadParams.pthread_ptr
  };
  // Ask the worker to start executing its pthread entry point function.
  worker.postMessage(msg, threadParams.transferList);
  return 0;
};

var runtimeKeepaliveCounter = 0;

var keepRuntimeAlive = () => noExitRuntime || runtimeKeepaliveCounter > 0;

var stackSave = () => _emscripten_stack_get_current();

var stackRestore = val => __emscripten_stack_restore(val);

var stackAlloc = sz => __emscripten_stack_alloc(sz);

/** @type {!Float64Array} */ var HEAPF64;

/** not-@type {!BigInt64Array} */ var HEAP64;

/** @type{function(number, (number|boolean), ...number)} */ var proxyToMainThread = (funcIndex, emAsmAddr, proxyMode, ...callArgs) => {
  // EM_ASM proxying is done by passing a pointer to the address of the EM_ASM
  // content as `emAsmAddr`.  JS library proxying is done by passing an index
  // into `proxiedJSCallArgs` as `funcIndex`. If `emAsmAddr` is non-zero then
  // `funcIndex` will be ignored.
  // Additional arguments are passed after the first three are the actual
  // function arguments.
  // The serialization buffer contains the number of call params, and then
  // all the args here.
  // We also pass 'proxyMode' to C separately, since C needs to look at it.
  // Allocate a buffer (on the stack), which will be copied if necessary by
  // the C code.
  // First passed parameter specifies the number of arguments to the function.
  // When BigInt support is enabled, we must handle types in a more complex
  // way, detecting at runtime if a value is a BigInt or not (as we have no
  // type info here). To do that, add a "prefix" before each value that
  // indicates if it is a BigInt, which effectively doubles the number of
  // values we serialize for proxying. TODO: pack this?
  var bufSize = 8 * callArgs.length * 2;
  var sp = stackSave();
  var args = stackAlloc(bufSize);
  var b = ((args) >>> 3);
  for (var arg of callArgs) {
    if (typeof arg == "bigint") {
      // The prefix is non-zero to indicate a bigint.
      (growMemViews(), HEAP64)[b++ >>> 0] = 1n;
      (growMemViews(), HEAP64)[b++ >>> 0] = arg;
    } else {
      // The prefix is zero to indicate a JS Number.
      (growMemViews(), HEAP64)[b++ >>> 0] = 0n;
      (growMemViews(), HEAPF64)[b++ >>> 0] = arg;
    }
  }
  var rtn = __emscripten_run_js_on_main_thread(funcIndex, emAsmAddr, bufSize, args, proxyMode);
  stackRestore(sp);
  return rtn;
};

function _proc_exit(code) {
  if (ENVIRONMENT_IS_PTHREAD) return proxyToMainThread(0, 0, 1, code);
  EXITSTATUS = code;
  if (!keepRuntimeAlive()) {
    PThread.terminateAllThreads();
    Module["onExit"]?.(code);
    ABORT = true;
  }
  quit_(code, new ExitStatus(code));
}

var runtimeKeepalivePop = () => {
  assert(runtimeKeepaliveCounter > 0);
  runtimeKeepaliveCounter -= 1;
};

function exitOnMainThread(returnCode) {
  if (ENVIRONMENT_IS_PTHREAD) return proxyToMainThread(1, 0, 0, returnCode);
  runtimeKeepalivePop();
  _exit(returnCode);
}

/** @param {boolean|number=} implicit */ var exitJS = (status, implicit) => {
  EXITSTATUS = status;
  checkUnflushedContent();
  if (ENVIRONMENT_IS_PTHREAD) {
    // implicit exit can never happen on a pthread
    assert(!implicit);
    // When running in a pthread we propagate the exit back to the main thread
    // where it can decide if the whole process should be shut down or not.
    // The pthread may have decided not to exit its own runtime, for example
    // because it runs a main loop, but that doesn't affect the main thread.
    exitOnMainThread(status);
    throw "unwind";
  }
  // if exit() was called explicitly, warn the user if the runtime isn't actually being shut down
  if (keepRuntimeAlive() && !implicit) {
    var msg = `program exited (with status: ${status}), but keepRuntimeAlive() is set (counter=${runtimeKeepaliveCounter}) due to an async operation, so halting execution but not exiting the runtime or preventing further async execution (you can use emscripten_force_exit, if you want to force a true shutdown)`;
    err(msg);
  }
  _proc_exit(status);
};

var _exit = exitJS;

var waitAsyncPolyfilled = (!Atomics.waitAsync || (globalThis.navigator?.userAgent && Number((navigator.userAgent.match(/Chrom(e|ium)\/([0-9]+)\./) || [])[2]) < 91));

function ptrToString(ptr) {
  assert(typeof ptr === "number", `ptrToString expects a number, got ${typeof ptr}`);
  // Convert to 32-bit unsigned value
  ptr >>>= 0;
  return "0x" + ptr.toString(16).padStart(8, "0");
}

var PThread = {
  unusedWorkers: [],
  tlsInitFunctions: [],
  pthreads: {},
  nextWorkerID: 1,
  init() {
    if ((!(ENVIRONMENT_IS_PTHREAD))) {
      PThread.initMainThread();
    }
  },
  initMainThread() {
    var pthreadPoolSize = 8;
    // Start loading up the Worker pool, if requested.
    while (pthreadPoolSize--) {
      PThread.allocateUnusedWorker();
    }
    // MINIMAL_RUNTIME takes care of calling loadWasmModuleToAllWorkers
    // in postamble_minimal.js
    addOnPreRun(async () => {
      var pthreadPoolReady = PThread.loadWasmModuleToAllWorkers();
      addRunDependency("loading-workers");
      await pthreadPoolReady;
      removeRunDependency("loading-workers");
    });
  },
  terminateAllThreads: () => {
    assert(!ENVIRONMENT_IS_PTHREAD, "terminateAllThreads() should only be called from the main thread");
    // Attempt to kill all workers.  Sadly (at least on the web) there is no
    // way to terminate a worker synchronously, or to be notified when a
    // worker is actually terminated.  This means there is some risk that
    // pthreads will continue to be executing after `worker.terminate` has
    // returned.  For this reason, we don't call `returnWorkerToPool` here or
    // free the underlying pthread data structures.
    for (var worker of Object.values(PThread.pthreads)) {
      terminateWorker(worker);
    }
    for (var worker of PThread.unusedWorkers) {
      terminateWorker(worker);
    }
    PThread.unusedWorkers = [];
    PThread.pthreads = {};
  },
  clearMailboxAwait: pthread_ptr => {
    if (!waitAsyncPolyfilled) {
      Atomics.notify((growMemViews(), HEAP32), ((pthread_ptr) >>> 2));
    }
  },
  terminateRuntime: () => {
    assert(!ENVIRONMENT_IS_PTHREAD, "terminateRuntime() should only be called from the main thread");
    PThread.terminateAllThreads();
    var pthread_ptr = _pthread_self();
    ___set_thread_state(0, 0, 0, 1);
    PThread.clearMailboxAwait(pthread_ptr);
  },
  returnWorkerToPool: worker => {
    // We don't want to run main thread queued calls here, since we are doing
    // some operations that leave the worker queue in an invalid state until
    // we are completely done (it would be bad if free() ends up calling a
    // queued pthread_create which looks at the global data structures we are
    // modifying). To achieve that, defer the free() until the very end, when
    // we are all done.
    var pthread_ptr = worker.pthread_ptr;
    delete PThread.pthreads[pthread_ptr];
    // Note: worker is intentionally not terminated so the pool can
    // dynamically grow.
    PThread.unusedWorkers.push(worker);
    // Not a running Worker anymore
    // Detach the worker from the pthread object, and return it to the
    // worker pool as an unused worker.
    worker.pthread_ptr = 0;
    if (ENVIRONMENT_IS_NODE) {
      // Once the proxied main thread has finished, mark it as weakly
      // referenced so that its existence does not prevent Node.js from
      // exiting.  This has no effect if the worker is already weakly
      // referenced.
      worker.unref();
    }
    // Clear any pending waitAsync waiter armed on this thread's struct
    // BEFORE freeing the memory so that memory recycled by malloc in another
    // thread will not have a window where a stale async waiter is still active.
    PThread.clearMailboxAwait(pthread_ptr);
    // Finally, free the underlying (and now-unused) pthread structure in
    // linear memory.
    __emscripten_thread_free_data(pthread_ptr);
  },
  threadInitTLS() {
    // Call thread init functions (these are the _emscripten_tls_init for each
    // module loaded.
    PThread.tlsInitFunctions.forEach(f => f());
  },
  loadWasmModuleToWorker: worker => new Promise(onFinishedLoading => {
    worker.onmessage = e => {
      var d = e.data;
      var cmd = d.cmd;
      // If this message is intended to a recipient that is not the main
      // thread, forward it to the target thread. This is currently only
      // used by `CMD_CHECK_MAILBOX`.
      if (d.targetThread && d.targetThread != _pthread_self()) {
        var targetWorker = PThread.pthreads[d.targetThread];
        if (!targetWorker) err(`worker sent message (${cmd}) to pthread (${d.targetThread}) that no longer exists`);
        targetWorker?.postMessage(d);
        return;
      }
      if (d === "setimmediate" || d === "_si") {
        // Worker wants to postMessage() to itself to implement setImmediate()
        // emulation.
        worker.postMessage(d);
        return;
      }
      switch (cmd) {
       case 4:
        checkMailbox();
        break;

       case 5:
        spawnThread(d);
        break;

       case 6:
        // cleanupThread needs to be run via callUserCallback since it calls
        // back into user code to free thread data. Without this it's possible
        // the unwind or ExitStatus exception could escape here.
        callUserCallback(() => cleanupThread(d.thread));
        break;

       case 3:
        if (ENVIRONMENT_IS_NODE && !worker.strongref) {
          // Once worker is loaded & idle, mark it as weakly referenced,
          // so that mere existence of a Worker in the pool does not prevent
          // Node.js from exiting the app.
          worker.unref();
        }
        onFinishedLoading(worker);
        break;

       case 8:
        // Message handler for Node.js specific out-of-order behavior:
        // https://github.com/nodejs/node/issues/59617
        // A pthread sent an uncaught exception event. Re-raise it on the main thread.
        worker.onerror(d.error);
        break;

       case 9:
        Module[d.handler](...d.args);
        break;

       default:
        // The received message looks like something that should be handled by this message
        // handler, (since there is a e.data.cmd field present), but is not one of the
        // recognized commands:
        if (cmd) err(`worker sent an unknown command ${cmd}`);
      }
    };
    worker.onerror = e => {
      var message = "worker sent an error!";
      if (worker.pthread_ptr) {
        message = `Pthread ${ptrToString(worker.pthread_ptr)} sent an error!`;
      }
      err(`${message} ${e.filename}:${e.lineno}: ${e.message}`);
      throw e;
    };
    if (ENVIRONMENT_IS_NODE) {
      worker.on("message", data => worker.onmessage({
        data
      }));
      worker.on("error", e => worker.onerror(e));
    }
    assert(wasmMemory instanceof WebAssembly.Memory, "wasmMemory should have been loaded by now");
    assert(wasmModule instanceof WebAssembly.Module, "wasmModule should have been loaded by now");
    // When running on a pthread, none of the incoming parameters on the module
    // object are present. Proxy known handlers back to the main thread if specified.
    var handlers = [];
    var knownHandlers = [ "onExit", "onAbort", "print", "printErr" ];
    for (var handler of knownHandlers) {
      if (Module.propertyIsEnumerable(handler)) {
        handlers.push(handler);
      }
    }
    // Ask the new worker to load up the Emscripten-compiled page. This is a heavy operation.
    worker.postMessage({
      cmd: 1,
      handlers,
      wasmMemory,
      wasmModule,
      workerID: worker.workerID
    });
  }),
  async loadWasmModuleToAllWorkers() {
    // Instantiation is synchronous in pthreads.
    if (ENVIRONMENT_IS_PTHREAD) {
      return;
    }
    let pthreadPoolReady = Promise.all(PThread.unusedWorkers.map(PThread.loadWasmModuleToWorker));
    return pthreadPoolReady;
  },
  allocateUnusedWorker() {
    var worker;
    // If we're using module output, use bundler-friendly pattern.
    // We need to generate the URL with import.meta.url as the base URL of the JS file
    // instead of just using new URL(import.meta.url) because bundlers only recognize
    // the first case in their bundling step. The latter ends up producing an invalid
    // URL to import from the server (e.g., for webpack the file:// path).
    // See https://github.com/webpack/webpack/issues/12638
    worker = new Worker(new URL("lucivy.js", import.meta.url), {
      "type": "module",
      // This is the way that we signal to the node worker that it is hosting
      // a pthread.
      "workerData": "em-pthread",
      // In WasmFS, close() is not proxied to the main thread. Suppress
      // warnings when a thread closes a file descriptor it didn't open.
      // See: https://github.com/emscripten-core/emscripten/issues/24731
      "trackUnmanagedFds": false,
      // This is the way that we signal to the Web Worker that it is hosting
      // a pthread.
      "name": "em-pthread-" + PThread.nextWorkerID
    });
    worker.workerID = PThread.nextWorkerID++;
    PThread.unusedWorkers.push(worker);
    return worker;
  },
  getNewWorker() {
    if (PThread.unusedWorkers.length == 0) {
      // PTHREAD_POOL_SIZE_STRICT should show a warning and, if set to level `2`, return from the function.
      var newWorker = PThread.allocateUnusedWorker();
      PThread.loadWasmModuleToWorker(newWorker);
    }
    return PThread.unusedWorkers.pop();
  }
};

var onPostRuns = [];

var dynCalls = {};

var dynCallLegacy = (sig, ptr, args) => {
  sig = sig.replace(/p/g, "i");
  assert(sig in dynCalls, `bad function pointer type - sig is not in dynCalls: '${sig}'`);
  if (args?.length) {
    // j (64-bit integer) is fine, and is implemented as a BigInt. Without
    // legalization, the number of parameters should match (j is not expanded
    // into two i's).
    assert(args.length === sig.length - 1);
  } else {
    assert(sig.length == 1);
  }
  var f = dynCalls[sig];
  return f(ptr, ...args);
};

function establishStackSpace(pthread_ptr) {
  var stackHigh = (growMemViews(), HEAPU32)[(((pthread_ptr) + (48)) >>> 2) >>> 0];
  var stackSize = (growMemViews(), HEAPU32)[(((pthread_ptr) + (52)) >>> 2) >>> 0];
  var stackLow = stackHigh - stackSize;
  assert(stackHigh != 0);
  assert(stackLow != 0);
  assert(stackHigh > stackLow, "stackHigh must be higher then stackLow");
  // Set stack limits used by `emscripten/stack.h` function.  These limits are
  // cached in wasm-side globals to make checks as fast as possible.
  _emscripten_stack_set_limits(stackHigh, stackLow);
  setStackLimits();
  // Call inside wasm module to set up the stack frame for this pthread in wasm module scope
  stackRestore(stackHigh);
  // Write the stack cookie last, after we have set up the proper bounds and
  // current position of the stack.
  writeStackCookie();
}

var invokeEntryPoint = (ptr, arg) => {
  // An old thread on this worker may have been canceled without returning the
  // `runtimeKeepaliveCounter` to zero. Reset it now so the new thread won't
  // be affected.
  runtimeKeepaliveCounter = 0;
  // Same for noExitRuntime.  The default for pthreads should always be false
  // otherwise pthreads would never complete and attempts to pthread_join to
  // them would block forever.
  // pthreads can still choose to set `noExitRuntime` explicitly, or
  // call emscripten_unwind_to_js_event_loop to extend their lifetime beyond
  // their main function.  See comment in src/runtime_pthread.js for more.
  noExitRuntime = 0;
  // pthread entry points are always of signature 'void *ThreadMain(void *arg)'
  // Native codebases sometimes spawn threads with other thread entry point
  // signatures, such as void ThreadMain(void *arg), void *ThreadMain(), or
  // void ThreadMain().  That is not acceptable per C/C++ specification, but
  // x86 compiler ABI extensions enable that to work. If you find the
  // following line to crash, either change the signature to "proper" void
  // *ThreadMain(void *arg) form, or try linking with the Emscripten linker
  // flag -sEMULATE_FUNCTION_POINTER_CASTS to add in emulation for this x86
  // ABI extension.
  var result = (a1 => dynCall_ii(ptr, a1))(arg);
  checkStackCookie();
  function finish(result) {
    // In MINIMAL_RUNTIME the noExitRuntime concept does not apply to
    // pthreads. To exit a pthread with live runtime, use the function
    // emscripten_unwind_to_js_event_loop() in the pthread body.
    if (keepRuntimeAlive()) {
      EXITSTATUS = result;
      return;
    }
    __emscripten_thread_exit(result);
  }
  finish(result);
};

var noExitRuntime = true;

var registerTLSInit = tlsInitFunc => PThread.tlsInitFunctions.push(tlsInitFunc);

var runtimeKeepalivePush = () => {
  runtimeKeepaliveCounter += 1;
};

var setStackLimits = () => {
  var stackLow = _emscripten_stack_get_base();
  var stackHigh = _emscripten_stack_get_end();
  ___set_stack_limits(stackLow, stackHigh);
};

var warnOnce = text => {
  warnOnce.shown ||= {};
  if (!warnOnce.shown[text]) {
    warnOnce.shown[text] = 1;
    if (ENVIRONMENT_IS_NODE) text = "warning: " + text;
    err(text);
  }
};

var wasmMemory;

var INT53_MAX = 9007199254740992;

var INT53_MIN = -9007199254740992;

var bigintToI53Checked = num => (num < INT53_MIN || num > INT53_MAX) ? NaN : Number(num);

var UTF8Decoder = globalThis.TextDecoder && new TextDecoder;

/**
   * heapOrArray is either a regular array, or a JavaScript typed array view.
   * @param {number} idx
   * @param {number=} maxBytesToRead
   * @param {boolean=} ignoreNul
   * @return {number}
   */ var findStringEnd = (heapOrArray, idx, maxBytesToRead, ignoreNul) => {
  var maxIdx = idx + maxBytesToRead;
  if (ignoreNul) return maxIdx;
  // TextDecoder needs to know the byte length in advance, it doesn't stop on
  // null terminator by itself.
  // As a tiny code save trick, compare idx against maxIdx using a negation,
  // so that maxBytesToRead=undefined/NaN means Infinity.
  while (heapOrArray[idx] && !(idx >= maxIdx)) ++idx;
  return idx;
};

/**
   * Given a pointer 'idx' to a null-terminated UTF8-encoded string in the given
   * array that contains uint8 values, returns a copy of that string as a
   * Javascript String object.
   * heapOrArray is either a regular array, or a JavaScript typed array view.
   * @param {number=} idx
   * @param {number=} maxBytesToRead
   * @param {boolean=} ignoreNul - If true, the function will not stop on a NUL character.
   * @return {string}
   */ var UTF8ArrayToString = (heapOrArray, idx = 0, maxBytesToRead, ignoreNul) => {
  idx >>>= 0;
  var endPtr = findStringEnd(heapOrArray, idx, maxBytesToRead, ignoreNul);
  // When using conditional TextDecoder, skip it for short strings as the overhead of the native call is not worth it.
  if (endPtr - idx > 16 && heapOrArray.buffer && UTF8Decoder) {
    return UTF8Decoder.decode(heapOrArray.buffer instanceof ArrayBuffer ? heapOrArray.subarray(idx, endPtr) : heapOrArray.slice(idx, endPtr));
  }
  var str = "";
  while (idx < endPtr) {
    // For UTF8 byte structure, see:
    // http://en.wikipedia.org/wiki/UTF-8#Description
    // https://www.ietf.org/rfc/rfc2279.txt
    // https://tools.ietf.org/html/rfc3629
    var u0 = heapOrArray[idx++];
    if (!(u0 & 128)) {
      str += String.fromCharCode(u0);
      continue;
    }
    var u1 = heapOrArray[idx++] & 63;
    if ((u0 & 224) == 192) {
      str += String.fromCharCode(((u0 & 31) << 6) | u1);
      continue;
    }
    var u2 = heapOrArray[idx++] & 63;
    if ((u0 & 240) == 224) {
      u0 = ((u0 & 15) << 12) | (u1 << 6) | u2;
    } else {
      if ((u0 & 248) != 240) warnOnce(`Invalid UTF-8 leading byte ${ptrToString(u0)} encountered when deserializing a UTF-8 string in wasm memory to a JS string!`);
      u0 = ((u0 & 7) << 18) | (u1 << 12) | (u2 << 6) | (heapOrArray[idx++] & 63);
    }
    if (u0 < 65536) {
      str += String.fromCharCode(u0);
    } else {
      var ch = u0 - 65536;
      str += String.fromCharCode(55296 | (ch >> 10), 56320 | (ch & 1023));
    }
  }
  return str;
};

/** @type {!Uint8Array} */ var HEAPU8;

/**
   * Given a pointer 'ptr' to a null-terminated UTF8-encoded string in the
   * emscripten HEAP, returns a copy of that string as a Javascript String object.
   *
   * @param {number} ptr
   * @param {number=} maxBytesToRead - An optional length that specifies the
   *   maximum number of bytes to read. You can omit this parameter to scan the
   *   string until the first 0 byte. If maxBytesToRead is passed, and the string
   *   at [ptr, ptr+maxBytesToReadr[ contains a null byte in the middle, then the
   *   string will cut short at that byte index.
   * @param {boolean=} ignoreNul - If true, the function will not stop on a NUL character.
   * @return {string}
   */ var UTF8ToString = (ptr, maxBytesToRead, ignoreNul) => {
  assert(typeof ptr == "number", `UTF8ToString expects a number (got ${typeof ptr})`);
  ptr >>>= 0;
  return ptr ? UTF8ArrayToString((growMemViews(), HEAPU8), ptr, maxBytesToRead, ignoreNul) : "";
};

function ___assert_fail(condition, filename, line, func) {
  condition >>>= 0;
  filename >>>= 0;
  func >>>= 0;
  return abort(`Assertion failed: ${UTF8ToString(condition)}, at: ` + [ filename ? UTF8ToString(filename) : "unknown filename", line, func ? UTF8ToString(func) : "unknown function" ]);
}

var ___call_sighandler = function(fp, sig) {
  fp >>>= 0;
  return (a1 => dynCall_vi(fp, a1))(sig);
};

var exceptionCaught = [];

var uncaughtExceptionCount = 0;

function ___cxa_begin_catch(ptr) {
  ptr >>>= 0;
  var info = new ExceptionInfo(ptr);
  if (!info.get_caught()) {
    info.set_caught(true);
    uncaughtExceptionCount--;
  }
  info.set_rethrown(false);
  exceptionCaught.push(info);
  return ___cxa_get_exception_ptr(ptr);
}

var exceptionLast = null;

class ExceptionInfo {
  // excPtr - Thrown object pointer to wrap. Metadata pointer is calculated from it.
  constructor(excPtr) {
    this.excPtr = excPtr;
    this.ptr = excPtr - 24;
  }
  set_type(type) {
    (growMemViews(), HEAPU32)[(((this.ptr) + (4)) >>> 2) >>> 0] = type;
  }
  get_type() {
    return (growMemViews(), HEAPU32)[(((this.ptr) + (4)) >>> 2) >>> 0];
  }
  set_destructor(destructor) {
    (growMemViews(), HEAPU32)[(((this.ptr) + (8)) >>> 2) >>> 0] = destructor;
  }
  get_destructor() {
    return (growMemViews(), HEAPU32)[(((this.ptr) + (8)) >>> 2) >>> 0];
  }
  set_caught(caught) {
    caught = caught ? 1 : 0;
    (growMemViews(), HEAP8)[(this.ptr) + (12) >>> 0] = caught;
  }
  get_caught() {
    return (growMemViews(), HEAP8)[(this.ptr) + (12) >>> 0] != 0;
  }
  set_rethrown(rethrown) {
    rethrown = rethrown ? 1 : 0;
    (growMemViews(), HEAP8)[(this.ptr) + (13) >>> 0] = rethrown;
  }
  get_rethrown() {
    return (growMemViews(), HEAP8)[(this.ptr) + (13) >>> 0] != 0;
  }
  // Initialize native structure fields. Should be called once after allocated.
  init(type, destructor) {
    this.set_adjusted_ptr(0);
    this.set_type(type);
    this.set_destructor(destructor);
  }
  set_adjusted_ptr(adjustedPtr) {
    (growMemViews(), HEAPU32)[(((this.ptr) + (16)) >>> 2) >>> 0] = adjustedPtr;
  }
  get_adjusted_ptr() {
    return (growMemViews(), HEAPU32)[(((this.ptr) + (16)) >>> 2) >>> 0];
  }
}

var setTempRet0 = val => __emscripten_tempret_set(val);

var findMatchingCatch = args => {
  var thrown = exceptionLast?.excPtr;
  if (!thrown) {
    // just pass through the null ptr
    setTempRet0(0);
    return 0;
  }
  var info = new ExceptionInfo(thrown);
  info.set_adjusted_ptr(thrown);
  var thrownType = info.get_type();
  if (!thrownType) {
    // just pass through the thrown ptr
    setTempRet0(0);
    return thrown;
  }
  // can_catch receives a **, add indirection
  // The different catch blocks are denoted by different types.
  // Due to inheritance, those types may not precisely match the
  // type of the thrown object. Find one which matches, and
  // return the type of the catch block which should be called.
  for (var caughtType of args) {
    if (!caughtType || caughtType === thrownType) {
      // Catch all clause matched or exactly the same type is caught
      break;
    }
    var adjusted_ptr_addr = info.ptr + 16;
    if (___cxa_can_catch(caughtType, thrownType, adjusted_ptr_addr)) {
      setTempRet0(caughtType);
      return thrown;
    }
  }
  setTempRet0(thrownType);
  return thrown;
};

function ___cxa_find_matching_catch_2() {
  return findMatchingCatch([]);
}

function ___cxa_find_matching_catch_3(arg0) {
  arg0 >>>= 0;
  return findMatchingCatch([ arg0 ]);
}

var getExceptionMessageCommon = ptr => {
  var sp = stackSave();
  var type_addr_addr = stackAlloc(4);
  var message_addr_addr = stackAlloc(4);
  ___get_exception_message(ptr, type_addr_addr, message_addr_addr);
  var type_addr = (growMemViews(), HEAPU32)[((type_addr_addr) >>> 2) >>> 0];
  var message_addr = (growMemViews(), HEAPU32)[((message_addr_addr) >>> 2) >>> 0];
  var type = UTF8ToString(type_addr);
  _free(type_addr);
  var message;
  if (message_addr) {
    message = UTF8ToString(message_addr);
    _free(message_addr);
  }
  stackRestore(sp);
  return [ type, message ];
};

var getExceptionMessage = exn => getExceptionMessageCommon(exn.excPtr);

var __Unwind_RaiseException = ex => {
  throw ex;
};

function ___cxa_throw(ptr, type, destructor) {
  ptr >>>= 0;
  type >>>= 0;
  destructor >>>= 0;
  var info = new ExceptionInfo(ptr);
  // Initialize ExceptionInfo content after it was allocated in __cxa_allocate_exception.
  info.init(type, destructor);
  ___cxa_increment_exception_refcount(ptr);
  ptr = exceptionLast = new CppException(ptr);
  uncaughtExceptionCount++;
  __Unwind_RaiseException(ptr);
}

function ___handle_stack_overflow(requested) {
  requested >>>= 0;
  var base = _emscripten_stack_get_base();
  var end = _emscripten_stack_get_end();
  abort(`stack overflow (Attempt to set SP to ${ptrToString(requested)}` + `, with stack limits [${ptrToString(end)} - ${ptrToString(base)}` + "]). If you require more stack space build with -sSTACK_SIZE=<bytes>");
}

function pthreadCreateProxied(pthread_ptr, attr, startRoutine, arg) {
  if (ENVIRONMENT_IS_PTHREAD) return proxyToMainThread(2, 0, 1, pthread_ptr, attr, startRoutine, arg);
  return ___pthread_create_js(pthread_ptr, attr, startRoutine, arg);
}

var _emscripten_has_threading_support = () => !!globalThis.SharedArrayBuffer;

function ___pthread_create_js(pthread_ptr, attr, startRoutine, arg) {
  pthread_ptr >>>= 0;
  attr >>>= 0;
  startRoutine >>>= 0;
  arg >>>= 0;
  if (!_emscripten_has_threading_support()) {
    dbg("pthread_create: environment does not support SharedArrayBuffer, pthreads are not available");
    return 6;
  }
  // List of JS objects that will transfer ownership to the Worker hosting the thread
  var transferList = [];
  var error = 0;
  // Synchronously proxy the thread creation to main thread if possible. If we
  // need to transfer ownership of objects, then proxy asynchronously via
  // postMessage.
  if (ENVIRONMENT_IS_PTHREAD && (!transferList.length || error)) {
    return pthreadCreateProxied(pthread_ptr, attr, startRoutine, arg);
  }
  // If on the main thread, and accessing Canvas/OffscreenCanvas failed, abort
  // with the detected error.
  if (error) return error;
  var threadParams = {
    startRoutine,
    pthread_ptr,
    arg,
    transferList
  };
  if (ENVIRONMENT_IS_PTHREAD) {
    // The prepopulated pool of web workers that can host pthreads is stored
    // in the main JS thread. Therefore if a pthread is attempting to spawn a
    // new thread, the thread creation must be deferred to the main JS thread.
    threadParams.cmd = 5;
    postMessage(threadParams, transferList);
    // When we defer thread creation this way, we have no way to detect thread
    // creation synchronously today, so we have to assume success and return 0.
    return 0;
  }
  // We are the main thread, so we have the pthread warmup pool in this
  // thread and can fire off JS thread creation directly ourselves.
  return spawnThread(threadParams);
}

var __Unwind_Resume = ex => {
  throw ex;
};

function ___resumeException(ptr) {
  ptr >>>= 0;
  ptr = exceptionLast ??= new CppException(ptr);
  __Unwind_Resume(ptr);
}

var __abort_js = () => abort("native code called abort()");

function __emscripten_init_main_thread_js(tb) {
  tb >>>= 0;
  var can_block = !ENVIRONMENT_IS_WEB;
  // Feature detect whether the main thread can block.
  try {
    Atomics.wait((growMemViews(), HEAP32), 0, 0, 0);
    can_block = true;
  } catch (e) {}
  // Pass the thread address to the native code where they are stored in wasm
  // globals which act as a form of TLS. Global constructors trying
  // to access this value will read the wrong value, but that is UB anyway.
  __emscripten_thread_init(tb, /*is_main=*/ !ENVIRONMENT_IS_WORKER, /*is_runtime=*/ 1, can_block, /*default_stacksize=*/ 2097152, /*start_profiling=*/ false);
  PThread.threadInitTLS();
}

var handleException = e => {
  // Certain exception types we do not treat as errors since they are used for
  // internal control flow.
  // 1. ExitStatus, which is thrown by exit()
  // 2. "unwind", which is thrown by emscripten_unwind_to_js_event_loop() and others
  //    that wish to return to JS event loop.
  if (e instanceof ExitStatus || e == "unwind") {
    return EXITSTATUS;
  }
  checkStackCookie();
  if (e instanceof WebAssembly.RuntimeError) {
    if (_emscripten_stack_get_current() <= 0) {
      err("Stack overflow detected.  You can try increasing -sSTACK_SIZE (currently set to 2097152)");
    }
  }
  quit_(1, e);
};

var maybeExit = () => {
  if (!keepRuntimeAlive()) {
    try {
      if (ENVIRONMENT_IS_PTHREAD) {
        // exit the current thread, but only if there is one active.
        // TODO(https://github.com/emscripten-core/emscripten/issues/25076):
        // Unify this check with the runtimeExited check above
        if (_pthread_self()) __emscripten_thread_exit(EXITSTATUS);
        return;
      }
      _exit(EXITSTATUS);
    } catch (e) {
      handleException(e);
    }
  }
};

var callUserCallback = func => {
  if (ABORT) {
    err("user callback triggered after runtime exited or application aborted.  Ignoring.");
    return;
  }
  try {
    return func();
  } catch (e) {
    handleException(e);
  } finally {
    maybeExit();
  }
};

function __emscripten_thread_mailbox_await(pthread_ptr) {
  pthread_ptr >>>= 0;
  if (!waitAsyncPolyfilled) {
    // Wait on the pthread's initial self-pointer field because it is easy and
    // safe to access from sending threads that need to notify the waiting
    // thread.
    // Note: Under wasm64 only the low 32-bit of the pthread_ptr are
    // read/compared here, but we don't actually care about the exact values
    // here as long as they match.
    var wait = Atomics.waitAsync((growMemViews(), HEAP32), ((pthread_ptr) >>> 2), pthread_ptr);
    assert(wait.async);
    wait.value.then(checkMailbox);
    var waitingAsync = pthread_ptr + 112;
    Atomics.store((growMemViews(), HEAP32), ((waitingAsync) >>> 2), 1);
  }
}

var checkMailbox = () => {
  // checkMailbox can be called after the pthread has shut down. See
  // Pthread.terminateRuntime().
  // In this case we return silently without re-registering using waitAsync.
  // Perhaps there is a more universal way we can detect runtime has exited.
  // TODO(https://github.com/emscripten-core/emscripten/issues/25076)
  var pthread_ptr = _pthread_self();
  if (!pthread_ptr) return;
  callUserCallback(() => {
    // If we are using Atomics.waitAsync as our notification mechanism, wait
    // for a notification before processing the mailbox to avoid missing any
    // work that could otherwise arrive after we've finished processing the
    // mailbox and before we're ready for the next notification.
    __emscripten_thread_mailbox_await(pthread_ptr);
    __emscripten_check_mailbox();
  });
};

function __emscripten_notify_mailbox_postmessage(targetThread, currThreadId) {
  targetThread >>>= 0;
  currThreadId >>>= 0;
  if (targetThread == currThreadId) {
    setTimeout(checkMailbox);
  } else if (ENVIRONMENT_IS_PTHREAD) {
    postMessage({
      targetThread,
      cmd: 4
    });
  } else {
    var worker = PThread.pthreads[targetThread];
    if (!worker) {
      err(`Cannot send message to thread with ID ${targetThread}, unknown thread ID!`);
      return;
    }
    worker.postMessage({
      cmd: 4
    });
  }
}

var proxiedJSCallArgs = [];

function __emscripten_receive_on_main_thread_js(funcIndex, emAsmAddr, callingThread, bufSize, args, ctx, ctxArgs) {
  emAsmAddr >>>= 0;
  callingThread >>>= 0;
  args >>>= 0;
  ctx >>>= 0;
  ctxArgs >>>= 0;
  // Sometimes we need to backproxy events to the calling thread (e.g.
  // HTML5 DOM events handlers such as
  // emscripten_set_mousemove_callback()), so keep track in a globally
  // accessible variable about the thread that initiated the proxying.
  proxiedJSCallArgs.length = 0;
  var b = ((args) >>> 3);
  var end = ((args + bufSize) >>> 3);
  while (b < end) {
    var arg;
    if ((growMemViews(), HEAP64)[b++ >>> 0]) {
      // It's a BigInt.
      arg = (growMemViews(), HEAP64)[b++ >>> 0];
    } else {
      // It's a Number.
      arg = (growMemViews(), HEAPF64)[b++ >>> 0];
    }
    proxiedJSCallArgs.push(arg);
  }
  // Proxied JS library funcs use funcIndex and EM_ASM functions use emAsmAddr
  assert(!emAsmAddr);
  var func = proxiedFunctionTable[funcIndex];
  assert(!(funcIndex && emAsmAddr));
  assert(func.length == proxiedJSCallArgs.length, "Call args mismatch in _emscripten_receive_on_main_thread_js");
  PThread.currentProxiedOperationCallerThread = callingThread;
  var rtn = func(...proxiedJSCallArgs);
  PThread.currentProxiedOperationCallerThread = 0;
  if (ctx) {
    rtn.then(rtn => __emscripten_run_js_on_main_thread_done(ctx, ctxArgs, rtn));
    return;
  }
  // Proxied functions can return any type except bigint.  All other types
  // coerce to f64/double (the return type of this function in C) but not
  // bigint.
  assert(typeof rtn != "bigint");
  return rtn;
}

var __emscripten_runtime_keepalive_clear = () => {
  noExitRuntime = false;
  runtimeKeepaliveCounter = 0;
};

function __emscripten_thread_cleanup(thread) {
  thread >>>= 0;
  // Called when a thread needs to be cleaned up so it can be reused.
  // A thread is considered reusable when it either returns from its
  // entry point, calls pthread_exit, or acts upon a cancellation.
  // Detached threads are responsible for calling this themselves,
  // otherwise pthread_join is responsible for calling this.
  if (!ENVIRONMENT_IS_PTHREAD) cleanupThread(thread); else postMessage({
    cmd: 6,
    thread
  });
}

function __emscripten_thread_set_strongref(thread) {
  thread >>>= 0;
  // Called when a thread needs to be strongly referenced.
  // Currently only used for:
  // - keeping the "main" thread alive in PROXY_TO_PTHREAD mode;
  // - crashed threads that need to propagate the uncaught exception
  //   back to the main thread.
  if (ENVIRONMENT_IS_NODE) {
    var worker = PThread.pthreads[thread];
    worker.ref();
    // Also, record that we called strongref, in case this function is called
    // bafore the 'loaded' callback from the thread (where we would normally
    // `unref` it.
    worker.strongref = 1;
  }
}

function __wasmfs_copy_preloaded_file_data(index, buffer) {
  buffer >>>= 0;
  return (growMemViews(), HEAPU8).set(wasmFSPreloadedFiles[index].fileData, buffer >>> 0);
}

var wasmFSPreloadedDirs = [];

var __wasmfs_get_num_preloaded_dirs = () => wasmFSPreloadedDirs.length;

var wasmFSPreloadedFiles = [];

var wasmFSPreloadingFlushed = false;

var __wasmfs_get_num_preloaded_files = () => {
  // When this method is called from WasmFS it means that we are about to
  // flush all the preloaded data, so mark that. (There is no call that
  // occurs at the end of that flushing, which would be more natural, but it
  // is fine to mark the flushing here as during the flushing itself no user
  // code can run, so nothing will check whether we have flushed or not.)
  wasmFSPreloadingFlushed = true;
  return wasmFSPreloadedFiles.length;
};

function __wasmfs_get_preloaded_child_path(index, childNameBuffer) {
  childNameBuffer >>>= 0;
  var s = wasmFSPreloadedDirs[index].childName;
  var len = lengthBytesUTF8(s) + 1;
  stringToUTF8(s, childNameBuffer, len);
}

var __wasmfs_get_preloaded_file_mode = index => wasmFSPreloadedFiles[index].mode;

function __wasmfs_get_preloaded_file_size(index) {
  return wasmFSPreloadedFiles[index].fileData.length;
}

function __wasmfs_get_preloaded_parent_path(index, parentPathBuffer) {
  parentPathBuffer >>>= 0;
  var s = wasmFSPreloadedDirs[index].parentPath;
  var len = lengthBytesUTF8(s) + 1;
  stringToUTF8(s, parentPathBuffer, len);
}

var lengthBytesUTF8 = str => {
  var len = 0;
  for (var i = 0; i < str.length; ++i) {
    // Gotcha: charCodeAt returns a 16-bit word that is a UTF-16 encoded code
    // unit, not a Unicode code point of the character! So decode
    // UTF16->UTF32->UTF8.
    // See http://unicode.org/faq/utf_bom.html#utf16-3
    var c = str.charCodeAt(i);
    // possibly a lead surrogate
    if (c <= 127) {
      len++;
    } else if (c <= 2047) {
      len += 2;
    } else if (c >= 55296 && c <= 57343) {
      len += 4;
      ++i;
    } else {
      len += 3;
    }
  }
  return len;
};

var stringToUTF8Array = (str, heap, outIdx, maxBytesToWrite) => {
  outIdx >>>= 0;
  assert(typeof str === "string", `stringToUTF8Array expects a string (got ${typeof str})`);
  // Parameter maxBytesToWrite is not optional. Negative values, 0, null,
  // undefined and false each don't write out any bytes.
  if (!(maxBytesToWrite > 0)) return 0;
  var startIdx = outIdx;
  var endIdx = outIdx + maxBytesToWrite - 1;
  // -1 for string null terminator.
  for (var i = 0; i < str.length; ++i) {
    // For UTF8 byte structure, see http://en.wikipedia.org/wiki/UTF-8#Description
    // and https://www.ietf.org/rfc/rfc2279.txt
    // and https://tools.ietf.org/html/rfc3629
    var u = str.codePointAt(i);
    if (u <= 127) {
      if (outIdx >= endIdx) break;
      heap[outIdx++ >>> 0] = u;
    } else if (u <= 2047) {
      if (outIdx + 1 >= endIdx) break;
      heap[outIdx++ >>> 0] = 192 | (u >> 6);
      heap[outIdx++ >>> 0] = 128 | (u & 63);
    } else if (u <= 65535) {
      if (outIdx + 2 >= endIdx) break;
      heap[outIdx++ >>> 0] = 224 | (u >> 12);
      heap[outIdx++ >>> 0] = 128 | ((u >> 6) & 63);
      heap[outIdx++ >>> 0] = 128 | (u & 63);
    } else {
      if (outIdx + 3 >= endIdx) break;
      if (u > 1114111) warnOnce(`Invalid Unicode code point ${ptrToString(u)} encountered when serializing a JS string to a UTF-8 string in wasm memory! (Valid unicode code points should be in range 0-0x10FFFF).`);
      heap[outIdx++ >>> 0] = 240 | (u >> 18);
      heap[outIdx++ >>> 0] = 128 | ((u >> 12) & 63);
      heap[outIdx++ >>> 0] = 128 | ((u >> 6) & 63);
      heap[outIdx++ >>> 0] = 128 | (u & 63);
      // Gotcha: if codePoint is over 0xFFFF, it is represented as a surrogate pair in UTF-16.
      // We need to manually skip over the second code unit for correct iteration.
      i++;
    }
  }
  // Null-terminate the pointer to the buffer.
  heap[outIdx >>> 0] = 0;
  return outIdx - startIdx;
};

var stringToUTF8 = (str, outPtr, maxBytesToWrite) => {
  assert(typeof maxBytesToWrite == "number", "stringToUTF8 requires a third parameter that specifies the length of the output buffer");
  return stringToUTF8Array(str, (growMemViews(), HEAPU8), outPtr, maxBytesToWrite);
};

function __wasmfs_get_preloaded_path_name(index, fileNameBuffer) {
  fileNameBuffer >>>= 0;
  var s = wasmFSPreloadedFiles[index].pathName;
  var len = lengthBytesUTF8(s) + 1;
  stringToUTF8(s, fileNameBuffer, len);
}

class HandleAllocator {
  allocated=[ undefined ];
  freelist=[];
  get(id) {
    assert(this.allocated[id] !== undefined, `invalid handle: ${id}`);
    return this.allocated[id];
  }
  has(id) {
    return this.allocated[id] !== undefined;
  }
  allocate(handle) {
    var id = this.freelist.pop() ?? this.allocated.length;
    this.allocated[id] = handle;
    return id;
  }
  free(id) {
    assert(this.allocated[id] !== undefined);
    // Set the slot to `undefined` rather than using `delete` here since
    // apparently arrays with holes in them can be less efficient.
    this.allocated[id] = undefined;
    this.freelist.push(id);
  }
}

var wasmfsOPFSAccessHandles = new HandleAllocator;

var wasmfsOPFSProxyFinish = ctx => {
  // When using pthreads the proxy needs to know when the work is finished.
  // When used with JSPI the work will be executed in an async block so there
  // is no need to notify when done.
  _emscripten_proxy_finish(ctx);
};

var __wasmfs_opfs_close_access = function(ctx, accessID, errPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    errPtr >>>= 0;
    let accessHandle = wasmfsOPFSAccessHandles.get(accessID);
    try {
      await accessHandle.close();
    } catch {
      let err = -29;
      (growMemViews(), HEAP32)[((errPtr) >>> 2) >>> 0] = err;
    }
    wasmfsOPFSAccessHandles.free(accessID);
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_close_access.isAsync = true;

var wasmfsOPFSBlobs = new HandleAllocator;

var __wasmfs_opfs_close_blob = blobID => {
  wasmfsOPFSBlobs.free(blobID);
};

var __wasmfs_opfs_flush_access = function(ctx, accessID, errPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    errPtr >>>= 0;
    let accessHandle = wasmfsOPFSAccessHandles.get(accessID);
    try {
      await accessHandle.flush();
    } catch {
      let err = -29;
      (growMemViews(), HEAP32)[((errPtr) >>> 2) >>> 0] = err;
    }
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_flush_access.isAsync = true;

var wasmfsOPFSDirectoryHandles = new HandleAllocator;

var __wasmfs_opfs_free_directory = dirID => {
  wasmfsOPFSDirectoryHandles.free(dirID);
};

var wasmfsOPFSFileHandles = new HandleAllocator;

var __wasmfs_opfs_free_file = fileID => {
  wasmfsOPFSFileHandles.free(fileID);
};

var wasmfsOPFSGetOrCreateFile = async (parent, name, create) => {
  let parentHandle = wasmfsOPFSDirectoryHandles.get(parent);
  let fileHandle;
  try {
    fileHandle = await parentHandle.getFileHandle(name, {
      create
    });
  } catch (e) {
    if (e.name === "NotFoundError") {
      return -20;
    }
    if (e.name === "TypeMismatchError") {
      return -31;
    }
    err("unexpected error:", e, e.stack);
    return -29;
  }
  return wasmfsOPFSFileHandles.allocate(fileHandle);
};

var wasmfsOPFSGetOrCreateDir = async (parent, name, create) => {
  let parentHandle = wasmfsOPFSDirectoryHandles.get(parent);
  let childHandle;
  try {
    childHandle = await parentHandle.getDirectoryHandle(name, {
      create
    });
  } catch (e) {
    if (e.name === "NotFoundError") {
      return -20;
    }
    if (e.name === "TypeMismatchError") {
      return -54;
    }
    err("unexpected error:", e, e.stack);
    return -29;
  }
  return wasmfsOPFSDirectoryHandles.allocate(childHandle);
};

var __wasmfs_opfs_get_child = function(ctx, parent, namePtr, childTypePtr, childIDPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    namePtr >>>= 0;
    childTypePtr >>>= 0;
    childIDPtr >>>= 0;
    let name = UTF8ToString(namePtr);
    let childType = 1;
    let childID = await wasmfsOPFSGetOrCreateFile(parent, name, false);
    if (childID == -31) {
      childType = 2;
      childID = await wasmfsOPFSGetOrCreateDir(parent, name, false);
    }
    (growMemViews(), HEAP32)[((childTypePtr) >>> 2) >>> 0] = childType;
    (growMemViews(), HEAP32)[((childIDPtr) >>> 2) >>> 0] = childID;
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_get_child.isAsync = true;

var __wasmfs_opfs_get_entries = function(ctx, dirID, entriesPtr, errPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    entriesPtr >>>= 0;
    errPtr >>>= 0;
    let dirHandle = wasmfsOPFSDirectoryHandles.get(dirID);
    // TODO: Use 'for await' once Acorn supports that.
    try {
      let iter = dirHandle.entries();
      for (let entry; entry = await iter.next(), !entry.done; ) {
        let [name, child] = entry.value;
        let sp = stackSave();
        let namePtr = stringToUTF8OnStack(name);
        let type = child.kind == "file" ? 1 : 2;
        __wasmfs_opfs_record_entry(entriesPtr, namePtr, type);
        stackRestore(sp);
      }
    } catch {
      let err = -29;
      (growMemViews(), HEAP32)[((errPtr) >>> 2) >>> 0] = err;
    }
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_get_entries.isAsync = true;

var __wasmfs_opfs_get_size_access = function(ctx, accessID, sizePtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    sizePtr >>>= 0;
    let accessHandle = wasmfsOPFSAccessHandles.get(accessID);
    let size;
    try {
      size = await accessHandle.getSize();
    } catch {
      size = -29;
    }
    (growMemViews(), HEAP64)[((sizePtr) >>> 3) >>> 0] = BigInt(size);
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_get_size_access.isAsync = true;

var __wasmfs_opfs_get_size_blob = function(blobID) {
  var ret = (() => wasmfsOPFSBlobs.get(blobID).size)();
  return BigInt(ret);
};

var __wasmfs_opfs_get_size_file = function(ctx, fileID, sizePtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    sizePtr >>>= 0;
    let fileHandle = wasmfsOPFSFileHandles.get(fileID);
    let size;
    try {
      size = (await fileHandle.getFile()).size;
    } catch {
      size = -29;
    }
    (growMemViews(), HEAP64)[((sizePtr) >>> 3) >>> 0] = BigInt(size);
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_get_size_file.isAsync = true;

var __wasmfs_opfs_init_root_directory = function(ctx) {
  let innerFunc = async () => {
    ctx >>>= 0;
    // allocated.length starts off as 1 since 0 is a reserved handle
    if (wasmfsOPFSDirectoryHandles.allocated.length == 1) {
      // Closure compiler errors on this as it does not recognize the OPFS
      // API yet, it seems. Unfortunately an existing annotation for this is in
      // the closure compiler codebase, and cannot be overridden in user code
      // (it complains on a duplicate type annotation), so just suppress it.
      /** @suppress {checkTypes} */ let root = await navigator.storage.getDirectory();
      wasmfsOPFSDirectoryHandles.allocated.push(root);
    }
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_init_root_directory.isAsync = true;

var __wasmfs_opfs_insert_directory = function(ctx, parent, namePtr, childIDPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    namePtr >>>= 0;
    childIDPtr >>>= 0;
    let name = UTF8ToString(namePtr);
    let childID = await wasmfsOPFSGetOrCreateDir(parent, name, true);
    (growMemViews(), HEAP32)[((childIDPtr) >>> 2) >>> 0] = childID;
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_insert_directory.isAsync = true;

var __wasmfs_opfs_insert_file = function(ctx, parent, namePtr, childIDPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    namePtr >>>= 0;
    childIDPtr >>>= 0;
    let name = UTF8ToString(namePtr);
    let childID = await wasmfsOPFSGetOrCreateFile(parent, name, true);
    (growMemViews(), HEAP32)[((childIDPtr) >>> 2) >>> 0] = childID;
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_insert_file.isAsync = true;

var __wasmfs_opfs_move_file = function(ctx, fileID, newParentID, namePtr, errPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    namePtr >>>= 0;
    errPtr >>>= 0;
    let name = UTF8ToString(namePtr);
    let fileHandle = wasmfsOPFSFileHandles.get(fileID);
    let newDirHandle = wasmfsOPFSDirectoryHandles.get(newParentID);
    try {
      await fileHandle.move(newDirHandle, name);
    } catch {
      let err = -29;
      (growMemViews(), HEAP32)[((errPtr) >>> 2) >>> 0] = err;
    }
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_move_file.isAsync = true;

var __wasmfs_opfs_open_access = function(ctx, fileID, accessIDPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    accessIDPtr >>>= 0;
    let fileHandle = wasmfsOPFSFileHandles.get(fileID);
    let accessID;
    try {
      let accessHandle;
      // TODO: Remove this once the Access Handles API has settled.
      // TODO: Closure is confused by this code that supports two versions of
      //       the same API, so suppress type checking on it.
      /** @suppress {checkTypes} */ var len = FileSystemFileHandle.prototype.createSyncAccessHandle.length;
      if (len == 0) {
        accessHandle = await fileHandle.createSyncAccessHandle();
      } else {
        accessHandle = await fileHandle.createSyncAccessHandle({
          mode: "in-place"
        });
      }
      accessID = wasmfsOPFSAccessHandles.allocate(accessHandle);
    } catch (e) {
      // TODO: Presumably only one of these will appear in the final API?
      if (e.name === "InvalidStateError" || e.name === "NoModificationAllowedError") {
        accessID = -2;
      } else {
        err("unexpected error:", e, e.stack);
        accessID = -29;
      }
    }
    (growMemViews(), HEAP32)[((accessIDPtr) >>> 2) >>> 0] = accessID;
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_open_access.isAsync = true;

var __wasmfs_opfs_open_blob = function(ctx, fileID, blobIDPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    blobIDPtr >>>= 0;
    let fileHandle = wasmfsOPFSFileHandles.get(fileID);
    let blobID;
    try {
      let blob = await fileHandle.getFile();
      blobID = wasmfsOPFSBlobs.allocate(blob);
    } catch (e) {
      if (e.name === "NotAllowedError") {
        blobID = -2;
      } else {
        err("unexpected error:", e, e.stack);
        blobID = -29;
      }
    }
    (growMemViews(), HEAP32)[((blobIDPtr) >>> 2) >>> 0] = blobID;
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_open_blob.isAsync = true;

var __wasmfs_opfs_read_access = function(accessID, bufPtr, len, pos) {
  let innerFunc = () => {
    bufPtr >>>= 0;
    pos = bigintToI53Checked(pos);
    let accessHandle = wasmfsOPFSAccessHandles.get(accessID);
    let data = (growMemViews(), HEAPU8).subarray(bufPtr >>> 0, bufPtr + len >>> 0);
    try {
      return accessHandle.read(data, {
        at: pos
      });
    } catch (e) {
      if (e.name == "TypeError") {
        return -28;
      }
      err("unexpected error:", e, e.stack);
      return -29;
    }
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_read_access.isAsync = true;

var __wasmfs_opfs_read_blob = function(ctx, blobID, bufPtr, len, pos, nreadPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    bufPtr >>>= 0;
    pos = bigintToI53Checked(pos);
    nreadPtr >>>= 0;
    let blob = wasmfsOPFSBlobs.get(blobID);
    let slice = blob.slice(pos, pos + len);
    let nread = 0;
    try {
      // TODO: Use ReadableStreamBYOBReader once
      // https://bugs.chromium.org/p/chromium/issues/detail?id=1189621 is
      // resolved.
      let buf = await slice.arrayBuffer();
      let data = new Uint8Array(buf);
      (growMemViews(), HEAPU8).set(data, bufPtr >>> 0);
      nread += data.length;
    } catch (e) {
      if (e instanceof RangeError) {
        nread = -21;
      } else {
        err("unexpected error:", e, e.stack);
        nread = -29;
      }
    }
    (growMemViews(), HEAP32)[((nreadPtr) >>> 2) >>> 0] = nread;
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_read_blob.isAsync = true;

var __wasmfs_opfs_remove_child = function(ctx, dirID, namePtr, errPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    namePtr >>>= 0;
    errPtr >>>= 0;
    let name = UTF8ToString(namePtr);
    let dirHandle = wasmfsOPFSDirectoryHandles.get(dirID);
    try {
      await dirHandle.removeEntry(name);
    } catch {
      let err = -29;
      (growMemViews(), HEAP32)[((errPtr) >>> 2) >>> 0] = err;
    }
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_remove_child.isAsync = true;

var __wasmfs_opfs_set_size_access = function(ctx, accessID, size, errPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    size = bigintToI53Checked(size);
    errPtr >>>= 0;
    let accessHandle = wasmfsOPFSAccessHandles.get(accessID);
    try {
      await accessHandle.truncate(size);
    } catch {
      let err = -29;
      (growMemViews(), HEAP32)[((errPtr) >>> 2) >>> 0] = err;
    }
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_set_size_access.isAsync = true;

var __wasmfs_opfs_set_size_file = function(ctx, fileID, size, errPtr) {
  let innerFunc = async () => {
    ctx >>>= 0;
    size = bigintToI53Checked(size);
    errPtr >>>= 0;
    let fileHandle = wasmfsOPFSFileHandles.get(fileID);
    try {
      let writable = await fileHandle.createWritable({
        keepExistingData: true
      });
      await writable.truncate(size);
      await writable.close();
    } catch {
      let err = -29;
      (growMemViews(), HEAP32)[((errPtr) >>> 2) >>> 0] = err;
    }
    wasmfsOPFSProxyFinish(ctx);
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_set_size_file.isAsync = true;

var __wasmfs_opfs_write_access = function(accessID, bufPtr, len, pos) {
  let innerFunc = () => {
    bufPtr >>>= 0;
    pos = bigintToI53Checked(pos);
    let accessHandle = wasmfsOPFSAccessHandles.get(accessID);
    let data = (growMemViews(), HEAPU8).subarray(bufPtr >>> 0, bufPtr + len >>> 0);
    try {
      return accessHandle.write(data, {
        at: pos
      });
    } catch (e) {
      if (e.name == "TypeError") {
        return -28;
      }
      err("unexpected error:", e, e.stack);
      return -29;
    }
  };
  return Asyncify.handleAsync(innerFunc);
};

__wasmfs_opfs_write_access.isAsync = true;

var FS_stdin_getChar_buffer = [];

/** @type {function(string, boolean=, number=)} */ var intArrayFromString = (stringy, dontAddNull, length) => {
  var len = length > 0 ? length : lengthBytesUTF8(stringy) + 1;
  var u8array = new Array(len);
  var numBytesWritten = stringToUTF8Array(stringy, u8array, 0, u8array.length);
  if (dontAddNull) u8array.length = numBytesWritten;
  return u8array;
};

var FS_stdin_getChar = () => {
  if (!FS_stdin_getChar_buffer.length) {
    var result = null;
    if (ENVIRONMENT_IS_NODE) {
      // we will read data by chunks of BUFSIZE
      var BUFSIZE = 256;
      var buf = Buffer.alloc(BUFSIZE);
      var bytesRead = 0;
      // For some reason we must suppress a closure warning here, even though
      // fd definitely exists on process.stdin, and is even the proper way to
      // get the fd of stdin,
      // https://github.com/nodejs/help/issues/2136#issuecomment-523649904
      // This started to happen after moving this logic out of library_tty.js,
      // so it is related to the surrounding code in some unclear manner.
      /** @suppress {missingProperties} */ var fd = process.stdin.fd;
      try {
        bytesRead = fs.readSync(fd, buf, 0, BUFSIZE);
      } catch (e) {
        // Cross-platform differences: on Windows, reading EOF throws an
        // exception, but on other OSes, reading EOF returns 0. Uniformize
        // behavior by treating the EOF exception to return 0.
        if (e.toString().includes("EOF")) bytesRead = 0; else throw e;
      }
      if (bytesRead > 0) {
        result = buf.slice(0, bytesRead).toString("utf-8");
      }
    } else if (globalThis.window?.prompt) {
      // Browser.
      result = window.prompt("Input: ");
      // returns null on cancel
      if (result !== null) {
        result += "\n";
      }
    } else {}
    if (!result) {
      return null;
    }
    FS_stdin_getChar_buffer = intArrayFromString(result, true);
  }
  return FS_stdin_getChar_buffer.shift();
};

var __wasmfs_stdin_get_char = () => {
  // Return the read character, or -1 to indicate EOF.
  var c = FS_stdin_getChar();
  if (typeof c === "number") {
    return c;
  }
  return -1;
};

var __wasmfs_thread_utils_heartbeat = function(queue) {
  queue >>>= 0;
  var intervalID = setInterval(() => {
    if (ABORT) {
      clearInterval(intervalID);
    } else {
      _emscripten_proxy_execute_queue(queue);
    }
  }, 50);
};

var _emscripten_get_now = () => performance.timeOrigin + performance.now();

var _emscripten_date_now = () => Date.now();

var nowIsMonotonic = 1;

var checkWasiClock = clock_id => clock_id >= 0 && clock_id <= 3;

function _clock_time_get(clk_id, ignored_precision, ptime) {
  ignored_precision = bigintToI53Checked(ignored_precision);
  ptime >>>= 0;
  if (!checkWasiClock(clk_id)) {
    return 28;
  }
  var now;
  // all wasi clocks but realtime are monotonic
  if (clk_id === 0) {
    now = _emscripten_date_now();
  } else if (nowIsMonotonic) {
    now = _emscripten_get_now();
  } else {
    return 52;
  }
  // "now" is in ms, and wasi times are in ns.
  var nsec = Math.round(now * 1e3 * 1e3);
  (growMemViews(), HEAP64)[((ptime) >>> 3) >>> 0] = BigInt(nsec);
  return 0;
}

var _emscripten_check_blocking_allowed = () => {
  if (ENVIRONMENT_IS_NODE) return;
  if (ENVIRONMENT_IS_WORKER) return;
  // Blocking in a worker/pthread is fine.
  warnOnce("Blocking on the main thread is very dangerous, see https://emscripten.org/docs/porting/pthreads.html#blocking-on-the-main-browser-thread");
};

function _emscripten_err(str) {
  str >>>= 0;
  return err(UTF8ToString(str));
}

var _emscripten_exit_with_live_runtime = () => {
  runtimeKeepalivePush();
  throw "unwind";
};

var jsStackTrace = () => (new Error).stack.toString();

/** @param {number=} flags */ var getCallstack = flags => {
  var callstack = jsStackTrace();
  if (flags & 8) {
    warnOnce("emscripten_log with EM_LOG_C_STACK no longer has any effect");
  }
  // Process all lines:
  var lines = callstack.split("\n");
  callstack = "";
  // Extract components of form:
  // '       Object._main@http://server.com:4324:12'
  var firefoxRe = new RegExp("\\s*(.*?)@(.*?):([0-9]+):([0-9]+)");
  // Extract components of form:
  // '    at Object._main (http://server.com/file.html:4324:12)'
  var chromeRe = new RegExp("\\s*at (.*?) \\((.*):(.*):(.*)\\)");
  for (var line of lines) {
    var symbolName = "";
    var file = "";
    var lineno = 0;
    var column = 0;
    var parts = chromeRe.exec(line);
    if (parts?.length == 5) {
      symbolName = parts[1];
      file = parts[2];
      lineno = parts[3];
      column = parts[4];
    } else {
      parts = firefoxRe.exec(line);
      if (parts?.length >= 4) {
        symbolName = parts[1];
        file = parts[2];
        lineno = parts[3];
        // Old Firefox doesn't carry column information, but in new FF30, it
        // is present. See https://bugzil.la/762556
        column = parts[4] | 0;
      } else {
        // Was not able to extract this line for demangling/sourcemapping
        // purposes. Output it as-is.
        callstack += line + "\n";
        continue;
      }
    }
    // Find the symbols in the callstack that corresponds to the functions that
    // report callstack information, and remove everything up to these from the
    // output.
    if (symbolName == "_emscripten_log" || symbolName == "_emscripten_get_callstack") {
      callstack = "";
      continue;
    }
    if ((flags & 24)) {
      if (flags & 64) {
        file = file.substring(file.replace(/\\/g, "/").lastIndexOf("/") + 1);
      }
      callstack += `    at ${symbolName} (${file}:${lineno}:${column})\n`;
    }
  }
  // Trim extra whitespace at the end of the output.
  callstack = callstack.replace(/\s+$/, "");
  return callstack;
};

function _emscripten_get_callstack(flags, str, maxbytes) {
  str >>>= 0;
  var callstack = getCallstack(flags);
  // User can query the required amount of bytes to hold the callstack.
  if (!str || maxbytes <= 0) {
    return lengthBytesUTF8(callstack) + 1;
  }
  // Output callstack string as C string to HEAP.
  var bytesWrittenExcludingNull = stringToUTF8(callstack, str, maxbytes);
  // Return number of bytes written, including null.
  return bytesWrittenExcludingNull + 1;
}

var getHeapMax = () => // Stay one Wasm page short of 4GB: while e.g. Chrome is able to allocate
// full 4GB Wasm memories, the size will wrap back to 0 bytes in Wasm side
// for any code that deals with heap sizes, which would require special
// casing all heap size related code to treat 0 specially.
4294901760;

function _emscripten_get_heap_max() {
  return getHeapMax();
}

var _emscripten_has_asyncify = () => 1;

var _emscripten_num_logical_cores = () => ENVIRONMENT_IS_NODE ? require("node:os").cpus().length : navigator["hardwareConcurrency"];

function _emscripten_out(str) {
  str >>>= 0;
  return out(UTF8ToString(str));
}

var alignMemory = (size, alignment) => {
  assert(alignment, "alignment argument is required");
  return Math.ceil(size / alignment) * alignment;
};

var growMemory = size => {
  var oldHeapSize = wasmMemory.buffer.byteLength;
  var pages = ((size - oldHeapSize + 65535) / 65536) | 0;
  try {
    // round size grow request up to wasm page size (fixed 64KB per spec)
    wasmMemory.grow(pages);
    // .grow() takes a delta compared to the previous size
    updateMemoryViews();
    return 1;
  } catch (e) {
    err(`growMemory: Attempted to grow heap from ${oldHeapSize} bytes to ${size} bytes, but got error: ${e}`);
  }
};

function _emscripten_resize_heap(requestedSize) {
  requestedSize >>>= 0;
  var oldSize = (growMemViews(), HEAPU8).length;
  // With multithreaded builds, races can happen (another thread might increase the size
  // in between), so return a failure, and let the caller retry.
  if (requestedSize <= oldSize) {
    return false;
  }
  // Memory resize rules:
  // 1.  Always increase heap size to at least the requested size, rounded up
  //     to next page multiple.
  // 2a. If MEMORY_GROWTH_LINEAR_STEP == -1, excessively resize the heap
  //     geometrically: increase the heap size according to
  //     MEMORY_GROWTH_GEOMETRIC_STEP factor (default +20%), At most
  //     overreserve by MEMORY_GROWTH_GEOMETRIC_CAP bytes (default 96MB).
  // 2b. If MEMORY_GROWTH_LINEAR_STEP != -1, excessively resize the heap
  //     linearly: increase the heap size by at least
  //     MEMORY_GROWTH_LINEAR_STEP bytes.
  // 3.  Max size for the heap is capped at 2048MB-WASM_PAGE_SIZE, or by
  //     MAXIMUM_MEMORY, or by ASAN limit, depending on which is smallest
  // 4.  If we were unable to allocate as much memory, it may be due to
  //     over-eager decision to excessively reserve due to (3) above.
  //     Hence if an allocation fails, cut down on the amount of excess
  //     growth, in an attempt to succeed to perform a smaller allocation.
  // A limit is set for how much we can grow. We should not exceed that
  // (the wasm binary specifies it, so if we tried, we'd fail anyhow).
  var maxHeapSize = getHeapMax();
  if (requestedSize > maxHeapSize) {
    err(`Cannot enlarge memory, requested ${requestedSize} bytes, but the limit is ${maxHeapSize} bytes!`);
    return false;
  }
  // Loop through potential heap size increases. If we attempt a too eager
  // reservation that fails, cut down on the attempted size and reserve a
  // smaller bump instead. (max 3 times, chosen somewhat arbitrarily)
  for (var cutDown = 1; cutDown <= 4; cutDown *= 2) {
    var overGrownHeapSize = oldSize * (1 + .2 / cutDown);
    // ensure geometric growth
    // but limit overreserving (default to capping at +96MB overgrowth at most)
    overGrownHeapSize = Math.min(overGrownHeapSize, requestedSize + 100663296);
    var newSize = Math.min(maxHeapSize, alignMemory(Math.max(requestedSize, overGrownHeapSize), 65536));
    var replacement = growMemory(newSize);
    if (replacement) {
      return true;
    }
  }
  err(`Failed to grow the heap from ${oldSize} bytes to ${newSize} bytes, not enough memory!`);
  return false;
}

var _emscripten_runtime_keepalive_check = keepRuntimeAlive;

var _emscripten_unwind_to_js_event_loop = () => {
  throw "unwind";
};

var ENV = {};

var getExecutableName = () => thisProgram;

var getEnvStrings = () => {
  if (!getEnvStrings.strings) {
    // Default values.
    var lang = (globalThis.navigator?.language ?? "C").replace("-", "_") + ".UTF-8";
    var env = {
      "USER": "web_user",
      "LOGNAME": "web_user",
      "PATH": "/",
      "PWD": "/",
      "HOME": "/home/web_user",
      "LANG": lang,
      "_": getExecutableName()
    };
    // Apply the user-provided values, if any.
    for (var x in ENV) {
      // x is a key in ENV; if ENV[x] is undefined, that means it was
      // explicitly set to be so. We allow user code to do that to
      // force variables with default values to remain unset.
      if (ENV[x] === undefined) delete env[x]; else env[x] = ENV[x];
    }
    var strings = [];
    for (var x in env) {
      strings.push(`${x}=${env[x]}`);
    }
    getEnvStrings.strings = strings;
  }
  return getEnvStrings.strings;
};

function _environ_get(__environ, environ_buf) {
  if (ENVIRONMENT_IS_PTHREAD) return proxyToMainThread(3, 0, 1, __environ, environ_buf);
  __environ >>>= 0;
  environ_buf >>>= 0;
  var bufSize = 0;
  var envp = 0;
  for (var string of getEnvStrings()) {
    var ptr = environ_buf + bufSize;
    (growMemViews(), HEAPU32)[(((__environ) + (envp)) >>> 2) >>> 0] = ptr;
    bufSize += stringToUTF8(string, ptr, Infinity) + 1;
    envp += 4;
  }
  return 0;
}

function _environ_sizes_get(penviron_count, penviron_buf_size) {
  if (ENVIRONMENT_IS_PTHREAD) return proxyToMainThread(4, 0, 1, penviron_count, penviron_buf_size);
  penviron_count >>>= 0;
  penviron_buf_size >>>= 0;
  var strings = getEnvStrings();
  (growMemViews(), HEAPU32)[((penviron_count) >>> 2) >>> 0] = strings.length;
  var bufSize = 0;
  for (var string of strings) {
    bufSize += lengthBytesUTF8(string) + 1;
  }
  (growMemViews(), HEAPU32)[((penviron_buf_size) >>> 2) >>> 0] = bufSize;
  return 0;
}

var initRandomFill = () => {
  // This block is not needed on v19+ since crypto.getRandomValues is builtin
  if (ENVIRONMENT_IS_NODE) {
    var nodeCrypto = require("node:crypto");
    return view => (nodeCrypto.randomFillSync(view), 0);
  }
  // like with most Web APIs, we can't use Web Crypto API directly on shared memory,
  // so we need to create an intermediate buffer and copy it to the destination
  return view => (view.set(crypto.getRandomValues(new Uint8Array(view.byteLength))), 
  0);
};

var randomFill = view => (randomFill = initRandomFill())(view);

function _random_get(buffer, size) {
  buffer >>>= 0;
  size >>>= 0;
  return randomFill((growMemViews(), HEAPU8).subarray(buffer >>> 0, buffer + size >>> 0));
}

var stringToUTF8OnStack = str => {
  var size = lengthBytesUTF8(str) + 1;
  var ret = stackAlloc(size);
  stringToUTF8(str, ret, size);
  return ret;
};

var wasmTableMirror = [];

var runAndAbortIfError = func => {
  try {
    return func();
  } catch (e) {
    abort(e);
  }
};

var createNamedFunction = (name, func) => Object.defineProperty(func, "name", {
  value: name
});

var Asyncify = {
  instrumentWasmImports(imports) {
    var importPattern = /^(invoke_.*|__asyncjs__.*)$/;
    for (let [x, original] of Object.entries(imports)) {
      if (typeof original == "function") {
        let isAsyncifyImport = original.isAsync || importPattern.test(x);
        imports[x] = (...args) => {
          var originalAsyncifyState = Asyncify.state;
          try {
            return original(...args);
          } finally {
            // Only asyncify-declared imports are allowed to change the
            // state.
            // Changing the state from normal to disabled is allowed (in any
            // function) as that is what shutdown does (and we don't have an
            // explicit list of shutdown imports).
            var changedToDisabled = originalAsyncifyState === Asyncify.State.Normal && Asyncify.state === Asyncify.State.Disabled;
            // invoke_* functions are allowed to change the state if we do
            // not ignore indirect calls.
            var ignoredInvoke = x.startsWith("invoke_") && true;
            if (Asyncify.state !== originalAsyncifyState && !isAsyncifyImport && !changedToDisabled && !ignoredInvoke) {
              abort(`import ${x} was not in ASYNCIFY_IMPORTS, but changed the state`);
            }
          }
        };
      }
    }
  },
  instrumentFunction(original) {
    var wrapper = (...args) => {
      Asyncify.exportCallStack.push(original);
      try {
        return original(...args);
      } finally {
        if (!ABORT) {
          var top = Asyncify.exportCallStack.pop();
          assert(top === original);
          Asyncify.maybeStopUnwind();
        }
      }
    };
    Asyncify.funcWrappers.set(original, wrapper);
    wrapper = createNamedFunction(`__asyncify_wrapper_${original.name}`, wrapper);
    return wrapper;
  },
  instrumentWasmExports(exports) {
    var ret = {};
    for (let [x, original] of Object.entries(exports)) {
      if (typeof original == "function") {
        var wrapper = Asyncify.instrumentFunction(original);
        ret[x] = wrapper;
      } else {
        ret[x] = original;
      }
    }
    return ret;
  },
  State: {
    Normal: 0,
    Unwinding: 1,
    Rewinding: 2,
    Disabled: 3
  },
  state: 0,
  StackSize: 1048576,
  currData: null,
  handleSleepReturnValue: 0,
  exportCallStack: [],
  callstackFuncToId: new Map,
  callStackIdToFunc: new Map,
  funcWrappers: new Map,
  callStackId: 0,
  asyncPromiseHandlers: null,
  sleepCallbacks: [],
  getCallStackId(func) {
    assert(func);
    if (!Asyncify.callstackFuncToId.has(func)) {
      var id = Asyncify.callStackId++;
      Asyncify.callstackFuncToId.set(func, id);
      Asyncify.callStackIdToFunc.set(id, func);
    }
    return Asyncify.callstackFuncToId.get(func);
  },
  maybeStopUnwind() {
    if (Asyncify.currData && Asyncify.state === Asyncify.State.Unwinding && !Asyncify.exportCallStack.length) {
      // We just finished unwinding.
      // Be sure to set the state before calling any other functions to avoid
      // possible infinite recursion here (For example in debug pthread builds
      // the dbg() function itself can call back into WebAssembly to get the
      // current pthread_self() pointer).
      Asyncify.state = Asyncify.State.Normal;
      runtimeKeepalivePush();
      // Keep the runtime alive so that a re-wind can be done later.
      runAndAbortIfError(_asyncify_stop_unwind);
      if (typeof Fibers != "undefined") {
        Fibers.trampoline();
      }
    }
  },
  whenDone() {
    assert(Asyncify.currData, "tried to wait for an async operation when none is in progress");
    assert(!Asyncify.asyncPromiseHandlers, "cannot have multiple async operations in flight at once");
    return new Promise((resolve, reject) => {
      Asyncify.asyncPromiseHandlers = {
        resolve,
        reject
      };
    });
  },
  allocateData() {
    // An asyncify data structure has three fields:
    //  0  current stack pos
    //  4  max stack pos
    //  8  id of function at bottom of the call stack (callStackIdToFunc[id] == wasm func)
    // The Asyncify ABI only interprets the first two fields, the rest is for the runtime.
    // We also embed a stack in the same memory region here, right next to the structure.
    // This struct is also defined as asyncify_data_t in emscripten/fiber.h
    var ptr = _malloc(12 + Asyncify.StackSize);
    Asyncify.setDataHeader(ptr, ptr + 12, Asyncify.StackSize);
    Asyncify.setDataRewindFunc(ptr);
    return ptr;
  },
  setDataHeader(ptr, stack, stackSize) {
    (growMemViews(), HEAPU32)[((ptr) >>> 2) >>> 0] = stack;
    (growMemViews(), HEAPU32)[(((ptr) + (4)) >>> 2) >>> 0] = stack + stackSize;
  },
  setDataRewindFunc(ptr) {
    var bottomOfCallStack = Asyncify.exportCallStack[0];
    assert(bottomOfCallStack, "exportCallStack is empty");
    var rewindId = Asyncify.getCallStackId(bottomOfCallStack);
    (growMemViews(), HEAP32)[(((ptr) + (8)) >>> 2) >>> 0] = rewindId;
  },
  getDataRewindFunc(ptr) {
    var id = (growMemViews(), HEAP32)[(((ptr) + (8)) >>> 2) >>> 0];
    var func = Asyncify.callStackIdToFunc.get(id);
    assert(func, `id ${id} not found in callStackIdToFunc`);
    return func;
  },
  doRewind(ptr) {
    var original = Asyncify.getDataRewindFunc(ptr);
    var func = Asyncify.funcWrappers.get(original);
    assert(original);
    assert(func);
    // Once we have rewound and the stack we no longer need to artificially
    // keep the runtime alive.
    runtimeKeepalivePop();
    return callUserCallback(func);
  },
  handleSleep(startAsync) {
    assert(Asyncify.state !== Asyncify.State.Disabled, "handleSleep called after Asyncify was shut down");
    if (ABORT) return;
    if (Asyncify.state === Asyncify.State.Normal) {
      // Prepare to sleep. Call startAsync, and see what happens:
      // if the code decided to call our callback synchronously,
      // then no async operation was in fact begun, and we don't
      // need to do anything.
      var reachedCallback = false;
      var reachedAfterCallback = false;
      startAsync((handleSleepReturnValue = 0) => {
        // old emterpretify API supported other stuff
        assert([ "undefined", "number", "boolean", "bigint" ].includes(typeof handleSleepReturnValue), `invalid type for handleSleepReturnValue: '${typeof handleSleepReturnValue}'`);
        if (ABORT) return;
        Asyncify.handleSleepReturnValue = handleSleepReturnValue;
        reachedCallback = true;
        if (!reachedAfterCallback) {
          // We are happening synchronously, so no need for async.
          return;
        }
        // This async operation did not happen synchronously, so we did
        // unwind. In that case there can be no compiled code on the stack,
        // as it might break later operations (we can rewind ok now, but if
        // we unwind again, we would unwind through the extra compiled code
        // too).
        assert(!Asyncify.exportCallStack.length, "waking up (starting to rewind) must be done from JS, without compiled code on the stack");
        Asyncify.state = Asyncify.State.Rewinding;
        runAndAbortIfError(() => _asyncify_start_rewind(Asyncify.currData));
        if (typeof MainLoop != "undefined" && MainLoop.func) {
          MainLoop.resume();
        }
        var asyncWasmReturnValue, isError = false;
        try {
          asyncWasmReturnValue = Asyncify.doRewind(Asyncify.currData);
        } catch (err) {
          asyncWasmReturnValue = err;
          isError = true;
        }
        // Track whether the return value was handled by any promise handlers.
        var handled = false;
        if (!Asyncify.currData) {
          // All asynchronous execution has finished.
          // `asyncWasmReturnValue` now contains the final
          // return value of the exported async WASM function.
          // Note: `asyncWasmReturnValue` is distinct from
          // `Asyncify.handleSleepReturnValue`.
          // `Asyncify.handleSleepReturnValue` contains the return
          // value of the last C function to have executed
          // `Asyncify.handleSleep()`, whereas `asyncWasmReturnValue`
          // contains the return value of the exported WASM function
          // that may have called C functions that
          // call `Asyncify.handleSleep()`.
          var asyncPromiseHandlers = Asyncify.asyncPromiseHandlers;
          if (asyncPromiseHandlers) {
            Asyncify.asyncPromiseHandlers = null;
            (isError ? asyncPromiseHandlers.reject : asyncPromiseHandlers.resolve)(asyncWasmReturnValue);
            handled = true;
          }
        }
        if (isError && !handled) {
          // If there was an error and it was not handled by now, we have no choice but to
          // rethrow that error into the global scope where it can be caught only by
          // `onerror` or `onunhandledpromiserejection`.
          throw asyncWasmReturnValue;
        }
      });
      reachedAfterCallback = true;
      if (!reachedCallback) {
        // A true async operation was begun; start a sleep.
        Asyncify.state = Asyncify.State.Unwinding;
        // TODO: reuse, don't alloc/free every sleep
        Asyncify.currData = Asyncify.allocateData();
        if (typeof MainLoop != "undefined" && MainLoop.func) {
          MainLoop.pause();
        }
        runAndAbortIfError(() => _asyncify_start_unwind(Asyncify.currData));
      }
    } else if (Asyncify.state === Asyncify.State.Rewinding) {
      // Stop a resume.
      Asyncify.state = Asyncify.State.Normal;
      runAndAbortIfError(_asyncify_stop_rewind);
      _free(Asyncify.currData);
      Asyncify.currData = null;
      // Call all sleep callbacks now that the sleep-resume is all done.
      Asyncify.sleepCallbacks.forEach(callUserCallback);
    } else {
      abort(`invalid state: ${Asyncify.state}`);
    }
    return Asyncify.handleSleepReturnValue;
  },
  handleAsync: startAsync => Asyncify.handleSleep(async wakeUp => {
    // TODO: add error handling as a second param when handleSleep implements it.
    wakeUp(await startAsync());
  })
};

var getCFunc = ident => {
  var func = Module["_" + ident];
  // closure exported function
  assert(func, `Cannot call unknown function ${ident}, make sure it is exported`);
  return func;
};

var writeArrayToMemory = (array, buffer) => {
  assert(array.length >= 0, "writeArrayToMemory array must have a length (should be an array or typed array)");
  (growMemViews(), HEAP8).set(array, buffer >>> 0);
};

/**
   * @param {string|null=} returnType
   * @param {Array=} argTypes
   * @param {Array=} args
   * @param {Object=} opts
   */ var ccall = (ident, returnType, argTypes, args, opts) => {
  // For fast lookup of conversion functions
  var toC = {
    "string": str => {
      var ret = 0;
      if (str !== null && str !== undefined && str !== 0) {
        // null string
        ret = stringToUTF8OnStack(str);
      }
      return ret;
    },
    "array": arr => {
      var ret = stackAlloc(arr.length);
      writeArrayToMemory(arr, ret);
      return ret;
    }
  };
  function convertReturnValue(ret) {
    if (returnType === "string") {
      return UTF8ToString(ret);
    }
    if (returnType === "pointer") return ret >>> 0;
    if (returnType === "boolean") return Boolean(ret);
    return ret;
  }
  var func = getCFunc(ident);
  var cArgs = [];
  var stack = 0;
  assert(returnType !== "array", 'return type should not be "array"');
  if (args) {
    for (var i = 0; i < args.length; i++) {
      var converter = toC[argTypes[i]];
      if (converter) {
        if (!stack) stack = stackSave();
        cArgs[i] = converter(args[i]);
      } else {
        cArgs[i] = args[i];
      }
    }
  }
  // Data for a previous async operation that was in flight before us.
  var previousAsync = Asyncify.currData;
  var ret = func(...cArgs);
  function onDone(ret) {
    runtimeKeepalivePop();
    if (stack) stackRestore(stack);
    return convertReturnValue(ret);
  }
  var asyncMode = opts?.async;
  // Keep the runtime alive through all calls. Note that this call might not be
  // async, but for simplicity we push and pop in all calls.
  runtimeKeepalivePush();
  if (Asyncify.currData != previousAsync) {
    // A change in async operation happened. If there was already an async
    // operation in flight before us, that is an error: we should not start
    // another async operation while one is active, and we should not stop one
    // either. The only valid combination is to have no change in the async
    // data (so we either had one in flight and left it alone, or we didn't have
    // one), or to have nothing in flight and to start one.
    assert(!(previousAsync && Asyncify.currData), "We cannot start an async operation when one is already in flight");
    assert(!(previousAsync && !Asyncify.currData), "We cannot stop an async operation in flight");
    // This is a new async operation. The wasm is paused and has unwound its stack.
    // We need to return a Promise that resolves the return value
    // once the stack is rewound and execution finishes.
    assert(asyncMode, `The call to ${ident} is running asynchronously. If this was intended, add the async option to the ccall/cwrap call.`);
    return Asyncify.whenDone().then(onDone);
  }
  ret = onDone(ret);
  // If this is an async ccall, ensure we return a promise
  if (asyncMode) return Promise.resolve(ret);
  return ret;
};

/**
   * @param {string=} returnType
   * @param {Array=} argTypes
   * @param {Object=} opts
   */ var cwrap = (ident, returnType, argTypes, opts) => (...args) => ccall(ident, returnType, argTypes, args, opts);

/** @type {!Int16Array} */ var HEAP16;

/** @type {!Float32Array} */ var HEAPF32;

/**
   * @param {number} ptr
   * @param {string} type
   */ function getValue(ptr, type = "i8") {
  if (type.endsWith("*")) type = "*";
  switch (type) {
   case "i1":
    return (growMemViews(), HEAP8)[ptr >>> 0];

   case "i8":
    return (growMemViews(), HEAP8)[ptr >>> 0];

   case "i16":
    return (growMemViews(), HEAP16)[((ptr) >>> 1) >>> 0];

   case "i32":
    return (growMemViews(), HEAP32)[((ptr) >>> 2) >>> 0];

   case "i64":
    return (growMemViews(), HEAP64)[((ptr) >>> 3) >>> 0];

   case "float":
    return (growMemViews(), HEAPF32)[((ptr) >>> 2) >>> 0];

   case "double":
    return (growMemViews(), HEAPF64)[((ptr) >>> 3) >>> 0];

   case "*":
    return (growMemViews(), HEAPU32)[((ptr) >>> 2) >>> 0];

   default:
    abort(`invalid type for getValue: ${type}`);
  }
}

PThread.init();

// End JS library code
// include: postlibrary.js
// This file is included after the automatically-generated JS library code
// but before the wasm module is created.
{
  // With WASM_ESM_INTEGRATION this has to happen at the top level and not
  // delayed until processModuleArgs.
  initMemory();
  // Begin ATMODULES hooks
  if (Module["noExitRuntime"]) noExitRuntime = Module["noExitRuntime"];
  if (Module["print"]) out = Module["print"];
  if (Module["printErr"]) err = Module["printErr"];
  Module["FS_createDataFile"] = FS.createDataFile;
  Module["FS_createPreloadedFile"] = FS.createPreloadedFile;
  // End ATMODULES hooks
  checkIncomingModuleAPI();
  if (Module["arguments"]) programArgs = Module["arguments"];
  if (Module["thisProgram"]) thisProgram = Module["thisProgram"];
  // Assertions on removed incoming Module JS APIs.
  assert(typeof Module["memoryInitializerPrefixURL"] == "undefined", "Module.memoryInitializerPrefixURL option was removed, use Module.locateFile instead");
  assert(typeof Module["pthreadMainPrefixURL"] == "undefined", "Module.pthreadMainPrefixURL option was removed, use Module.locateFile instead");
  assert(typeof Module["cdInitializerPrefixURL"] == "undefined", "Module.cdInitializerPrefixURL option was removed, use Module.locateFile instead");
  assert(typeof Module["filePackagePrefixURL"] == "undefined", "Module.filePackagePrefixURL option was removed, use Module.locateFile instead");
  assert(typeof Module["read"] == "undefined", "Module.read option was removed");
  assert(typeof Module["readAsync"] == "undefined", "Module.readAsync option was removed (modify readAsync in JS)");
  assert(typeof Module["readBinary"] == "undefined", "Module.readBinary option was removed (modify readBinary in JS)");
  assert(typeof Module["setWindowTitle"] == "undefined", "Module.setWindowTitle option was removed (modify emscripten_set_window_title in JS)");
  assert(typeof Module["TOTAL_MEMORY"] == "undefined", "Module.TOTAL_MEMORY has been renamed Module.INITIAL_MEMORY");
  assert(typeof Module["ENVIRONMENT"] == "undefined", "Module.ENVIRONMENT has been deprecated. To force the environment, use the ENVIRONMENT compile-time option (for example, -sENVIRONMENT=web or -sENVIRONMENT=node)");
  assert(typeof Module["STACK_SIZE"] == "undefined", "STACK_SIZE can no longer be set at runtime.  Use -sSTACK_SIZE at link time");
  var preInit = Module["preInit"];
  if (preInit) {
    if (typeof preInit == "function") Module["preInit"] = preInit = [ preInit ];
    // Written as a loop so that preInit functions that themselves add more
    // preInit functions.  Is this actually needed?
    while (preInit.length > 0) {
      preInit.shift()();
    }
  }
  consumedModuleProp("preInit");
}

// Begin runtime exports
Module["ccall"] = ccall;

Module["cwrap"] = cwrap;

Module["getValue"] = getValue;

Module["UTF8ToString"] = UTF8ToString;

Module["stringToUTF8"] = stringToUTF8;

Module["lengthBytesUTF8"] = lengthBytesUTF8;

var missingLibrarySymbols = [ "writeI53ToI64", "writeI53ToI64Clamped", "writeI53ToI64Signaling", "writeI53ToU64Clamped", "writeI53ToU64Signaling", "readI53FromI64", "readI53FromU64", "convertI32PairToI53", "convertI32PairToI53Checked", "convertU32PairToI53", "getTempRet0", "zeroMemory", "withStackSave", "strError", "inetPton4", "inetNtop4", "inetPton6", "inetNtop6", "readSockaddr", "writeSockaddr", "readEmAsmArgs", "jstoi_q", "autoResumeAudioContext", "getDynCaller", "asyncLoad", "asmjsMangle", "mmapAlloc", "getUniqueRunDependency", "addOnInit", "addOnPostCtor", "addOnPreMain", "addOnExit", "STACK_SIZE", "STACK_ALIGN", "POINTER_SIZE", "ASSERTIONS", "convertJsFunctionToWasm", "getEmptyTableSlot", "updateTableMap", "getFunctionAddress", "addFunction", "removeFunction", "setValue", "intArrayToString", "AsciiToString", "stringToAscii", "UTF16ToString", "stringToUTF16", "lengthBytesUTF16", "UTF32ToString", "stringToUTF32", "lengthBytesUTF32", "stringToNewUTF8", "registerKeyEventCallback", "maybeCStringToJsString", "findEventTarget", "getBoundingClientRect", "fillMouseEventData", "registerMouseEventCallback", "registerWheelEventCallback", "registerUiEventCallback", "registerFocusEventCallback", "fillDeviceOrientationEventData", "registerDeviceOrientationEventCallback", "fillDeviceMotionEventData", "registerDeviceMotionEventCallback", "screenOrientation", "fillOrientationChangeEventData", "registerOrientationChangeEventCallback", "fillFullscreenChangeEventData", "registerFullscreenChangeEventCallback", "callCanvasResizedCallback", "JSEvents_requestFullscreen", "JSEvents_resizeCanvasForFullscreen", "registerRestoreOldStyle", "hideEverythingExceptGivenElement", "restoreHiddenElements", "setLetterbox", "currentFullscreenStrategy", "softFullscreenResizeWebGLRenderTarget", "doRequestFullscreen", "fillPointerlockChangeEventData", "registerPointerlockChangeEventCallback", "registerPointerlockErrorEventCallback", "requestPointerLock", "fillVisibilityChangeEventData", "registerVisibilityChangeEventCallback", "registerTouchEventCallback", "fillGamepadEventData", "registerGamepadEventCallback", "registerBeforeUnloadEventCallback", "fillBatteryEventData", "registerBatteryEventCallback", "setCanvasElementSizeCallingThread", "setCanvasElementSizeMainThread", "setCanvasElementSize", "getCanvasSizeCallingThread", "getCanvasSizeMainThread", "getCanvasElementSize", "convertPCtoSourceLocation", "flush_NO_FILESYSTEM", "wasiRightsToMuslOFlags", "wasiOFlagsToMuslOFlags", "safeSetTimeout", "setImmediateWrapped", "safeRequestAnimationFrame", "clearImmediateWrapped", "registerPostMainLoop", "registerPreMainLoop", "getPromise", "makePromise", "addPromise", "idsToPromises", "makePromiseCallback", "incrementUncaughtExceptionCount", "decrementUncaughtExceptionCount", "Browser_asyncPrepareDataCounter", "isLeapYear", "ydayFromDate", "arraySum", "addDays", "FS_createPreloadedFile", "FS_preloadFile", "FS_modeStringToFlags", "FS_getMode", "FS_fileDataToTypedArray", "FS_unlink", "FS_createDataFile", "FS_mknod", "FS_create", "FS_writeFile", "FS_mkdir", "FS_mkdirTree", "wasmfsNodeConvertNodeCode", "wasmfsTry", "wasmfsNodeFixStat", "wasmfsNodeLstat", "wasmfsNodeFstat", "heapObjectForWebGLType", "toTypedArrayIndex", "webgl_enable_ANGLE_instanced_arrays", "webgl_enable_OES_vertex_array_object", "webgl_enable_WEBGL_draw_buffers", "webgl_enable_WEBGL_multi_draw", "webgl_enable_EXT_polygon_offset_clamp", "webgl_enable_EXT_clip_control", "webgl_enable_WEBGL_polygon_mode", "emscriptenWebGLGet", "computeUnpackAlignedImageSize", "colorChannelsInGlTextureFormat", "emscriptenWebGLGetTexPixelData", "emscriptenWebGLGetUniform", "webglGetProgramUniformLocation", "webglGetUniformLocation", "webglPrepareUniformLocationsBeforeFirstUse", "webglGetLeftBracePos", "emscriptenWebGLGetVertexAttrib", "__glGetActiveAttribOrUniform", "writeGLArray", "emscripten_webgl_destroy_context_before_on_calling_thread", "registerWebGlEventCallback", "writeStringToMemory", "writeAsciiToMemory", "allocateUTF8", "allocateUTF8OnStack", "demangle", "stackTrace", "getNativeTypeSize" ];

missingLibrarySymbols.forEach(missingLibrarySymbol);

var unexportedSymbols = [ "run", "out", "err", "callMain", "abort", "wasmExports", "writeStackCookie", "checkStackCookie", "INT53_MAX", "INT53_MIN", "bigintToI53Checked", "HEAP8", "HEAP16", "HEAPU16", "HEAP32", "HEAPU32", "HEAPF32", "HEAPF64", "HEAP64", "HEAPU64", "stackSave", "stackRestore", "stackAlloc", "setTempRet0", "createNamedFunction", "ptrToString", "exitJS", "getHeapMax", "growMemory", "ENV", "setStackLimits", "ERRNO_CODES", "DNS", "Protocols", "Sockets", "timers", "warnOnce", "readEmAsmArgsArray", "getExecutableName", "dynCallLegacy", "dynCall", "handleException", "keepRuntimeAlive", "runtimeKeepalivePush", "runtimeKeepalivePop", "callUserCallback", "maybeExit", "alignMemory", "HandleAllocator", "wasmTable", "wasmMemory", "noExitRuntime", "addRunDependency", "removeRunDependency", "addOnPreRun", "addOnPostRun", "freeTableIndexes", "functionsInTableMap", "PATH", "PATH_FS", "UTF8Decoder", "UTF8ArrayToString", "stringToUTF8Array", "intArrayFromString", "UTF16Decoder", "stringToUTF8OnStack", "writeArrayToMemory", "JSEvents", "specialHTMLTargets", "findCanvasEventTarget", "restoreOldWindowedStyle", "jsStackTrace", "getCallstack", "UNWIND_CACHE", "ExitStatus", "getEnvStrings", "checkWasiClock", "initRandomFill", "randomFill", "emSetImmediate", "emClearImmediate_deps", "emClearImmediate", "promiseMap", "uncaughtExceptionCount", "exceptionLast", "exceptionCaught", "ExceptionInfo", "findMatchingCatch", "getExceptionMessageCommon", "incrementExceptionRefcount", "decrementExceptionRefcount", "getExceptionMessage", "Browser", "requestFullscreen", "setCanvasSize", "getUserMedia", "createContext", "getPreloadedImageData__data", "wget", "MONTH_DAYS_REGULAR", "MONTH_DAYS_LEAP", "MONTH_DAYS_REGULAR_CUMULATIVE", "MONTH_DAYS_LEAP_CUMULATIVE", "preloadPlugins", "FS_stdin_getChar_buffer", "FS_stdin_getChar", "FS_createPath", "FS_createDevice", "FS_readFile", "MEMFS", "wasmFSPreloadedFiles", "wasmFSPreloadedDirs", "wasmFSPreloadingFlushed", "wasmFSDevices", "wasmFSDeviceStreams", "FS", "wasmFS$JSMemoryFiles", "wasmFS$backends", "wasmFS$JSMemoryRanges", "wasmfsNodeIsWindows", "wasmfsOPFSDirectoryHandles", "wasmfsOPFSFileHandles", "wasmfsOPFSAccessHandles", "wasmfsOPFSBlobs", "wasmfsOPFSProxyFinish", "wasmfsOPFSGetOrCreateFile", "wasmfsOPFSGetOrCreateDir", "tempFixedLengthArray", "miniTempWebGLFloatBuffers", "miniTempWebGLIntBuffers", "GL", "AL", "GLUT", "EGL", "GLEW", "IDBStore", "runAndAbortIfError", "Asyncify", "Fibers", "SDL", "SDL_gfx", "waitAsyncPolyfilled", "print", "printErr", "jstoi_s", "PThread", "terminateWorker", "cleanupThread", "registerTLSInit", "spawnThread", "exitOnMainThread", "proxyToMainThread", "proxiedJSCallArgs", "invokeEntryPoint", "checkMailbox" ];

unexportedSymbols.forEach(unexportedRuntimeSymbol);

// End runtime exports
// Begin JS library exports
// End JS library exports
// end include: postlibrary.js
// proxiedFunctionTable specifies the list of functions that can be called
// either synchronously or asynchronously from other threads in postMessage()d
// or internally queued events. This way a pthread in a Worker can synchronously
// access e.g. the DOM on the main thread.
var proxiedFunctionTable = [ _proc_exit, exitOnMainThread, pthreadCreateProxied, _environ_get, _environ_sizes_get ];

function checkIncomingModuleAPI() {
  ignoredModuleProp("fetchSettings");
  ignoredModuleProp("logReadFiles");
  ignoredModuleProp("loadSplitModule");
  ignoredModuleProp("onMalloc");
  ignoredModuleProp("onRealloc");
  ignoredModuleProp("onFree");
  ignoredModuleProp("onSbrkGrow");
  ignoredModuleProp("onCOSCacheHit");
  ignoredModuleProp("onCOSCacheMiss");
  ignoredModuleProp("onCOSStore");
  ignoredModuleProp("GL_MAX_TEXTURE_IMAGE_UNITS");
  ignoredModuleProp("SDL_canPlayWithWebAudio");
  ignoredModuleProp("SDL_numSimultaneouslyQueuedBuffers");
  ignoredModuleProp("freePreloadedMediaOnUse");
  ignoredModuleProp("preinitializedWebGLContext");
  ignoredModuleProp("keyboardListeningElement");
  ignoredModuleProp("doNotCaptureKeyboard");
  ignoredModuleProp("extraStackTrace");
  ignoredModuleProp("preloadPlugins");
  ignoredModuleProp("preMainLoop");
  ignoredModuleProp("postMainLoop");
  ignoredModuleProp("forcedAspectRatio");
  ignoredModuleProp("mainScriptUrlOrBlob");
  ignoredModuleProp("onFullScreen");
  ignoredModuleProp("INITIAL_MEMORY");
  ignoredModuleProp("wasmMemory");
  ignoredModuleProp("wasmBinary");
}

// Imports from the Wasm binary.
var _main = Module["_main"] = makeInvalidEarlyAccess("_main");

var _lucivy_add = Module["_lucivy_add"] = makeInvalidEarlyAccess("_lucivy_add");

var _lucivy_add_many = Module["_lucivy_add_many"] = makeInvalidEarlyAccess("_lucivy_add_many");

var _lucivy_apply_sharded_delta = Module["_lucivy_apply_sharded_delta"] = makeInvalidEarlyAccess("_lucivy_apply_sharded_delta");

var _lucivy_close = Module["_lucivy_close"] = makeInvalidEarlyAccess("_lucivy_close");

var _lucivy_commit = Module["_lucivy_commit"] = makeInvalidEarlyAccess("_lucivy_commit");

var _lucivy_commit_async = Module["_lucivy_commit_async"] = makeInvalidEarlyAccess("_lucivy_commit_async");

var _lucivy_commit_finish = Module["_lucivy_commit_finish"] = makeInvalidEarlyAccess("_lucivy_commit_finish");

var _lucivy_commit_status_ptr = Module["_lucivy_commit_status_ptr"] = makeInvalidEarlyAccess("_lucivy_commit_status_ptr");

var _lucivy_configure = Module["_lucivy_configure"] = makeInvalidEarlyAccess("_lucivy_configure");

var _lucivy_create = Module["_lucivy_create"] = makeInvalidEarlyAccess("_lucivy_create");

var _lucivy_destroy = Module["_lucivy_destroy"] = makeInvalidEarlyAccess("_lucivy_destroy");

var _lucivy_drain_merges = Module["_lucivy_drain_merges"] = makeInvalidEarlyAccess("_lucivy_drain_merges");

var _lucivy_dump_mermaid = Module["_lucivy_dump_mermaid"] = makeInvalidEarlyAccess("_lucivy_dump_mermaid");

var _lucivy_dump_state = Module["_lucivy_dump_state"] = makeInvalidEarlyAccess("_lucivy_dump_state");

var _lucivy_dump_wait_graph = Module["_lucivy_dump_wait_graph"] = makeInvalidEarlyAccess("_lucivy_dump_wait_graph");

var _lucivy_dump_wait_graph_text = Module["_lucivy_dump_wait_graph_text"] = makeInvalidEarlyAccess("_lucivy_dump_wait_graph_text");

var _lucivy_export_sharded_delta = Module["_lucivy_export_sharded_delta"] = makeInvalidEarlyAccess("_lucivy_export_sharded_delta");

var _lucivy_export_snapshot = Module["_lucivy_export_snapshot"] = makeInvalidEarlyAccess("_lucivy_export_snapshot");

var _lucivy_export_stats = Module["_lucivy_export_stats"] = makeInvalidEarlyAccess("_lucivy_export_stats");

var _lucivy_import_file = Module["_lucivy_import_file"] = makeInvalidEarlyAccess("_lucivy_import_file");

var _lucivy_import_snapshot = Module["_lucivy_import_snapshot"] = makeInvalidEarlyAccess("_lucivy_import_snapshot");

var _lucivy_log_ring_ptr = Module["_lucivy_log_ring_ptr"] = makeInvalidEarlyAccess("_lucivy_log_ring_ptr");

var _lucivy_log_ring_size = Module["_lucivy_log_ring_size"] = makeInvalidEarlyAccess("_lucivy_log_ring_size");

var _lucivy_merge_stats = Module["_lucivy_merge_stats"] = makeInvalidEarlyAccess("_lucivy_merge_stats");

var _lucivy_num_docs = Module["_lucivy_num_docs"] = makeInvalidEarlyAccess("_lucivy_num_docs");

var _lucivy_open = Module["_lucivy_open"] = makeInvalidEarlyAccess("_lucivy_open");

var _lucivy_open_begin = Module["_lucivy_open_begin"] = makeInvalidEarlyAccess("_lucivy_open_begin");

var _lucivy_open_finish = Module["_lucivy_open_finish"] = makeInvalidEarlyAccess("_lucivy_open_finish");

var _lucivy_query_warnings = Module["_lucivy_query_warnings"] = makeInvalidEarlyAccess("_lucivy_query_warnings");

var _lucivy_read_logs = Module["_lucivy_read_logs"] = makeInvalidEarlyAccess("_lucivy_read_logs");

var _lucivy_remove = Module["_lucivy_remove"] = makeInvalidEarlyAccess("_lucivy_remove");

var _lucivy_schema_json = Module["_lucivy_schema_json"] = makeInvalidEarlyAccess("_lucivy_schema_json");

var _lucivy_search = Module["_lucivy_search"] = makeInvalidEarlyAccess("_lucivy_search");

var _lucivy_search_filtered = Module["_lucivy_search_filtered"] = makeInvalidEarlyAccess("_lucivy_search_filtered");

var _lucivy_search_with_global_stats = Module["_lucivy_search_with_global_stats"] = makeInvalidEarlyAccess("_lucivy_search_with_global_stats");

var _lucivy_shard_versions = Module["_lucivy_shard_versions"] = makeInvalidEarlyAccess("_lucivy_shard_versions");

var _lucivy_test_condvar = Module["_lucivy_test_condvar"] = makeInvalidEarlyAccess("_lucivy_test_condvar");

var _lucivy_test_coop = Module["_lucivy_test_coop"] = makeInvalidEarlyAccess("_lucivy_test_coop");

var _lucivy_update = Module["_lucivy_update"] = makeInvalidEarlyAccess("_lucivy_update");

var __emscripten_tls_init = makeInvalidEarlyAccess("__emscripten_tls_init");

var _pthread_self = makeInvalidEarlyAccess("_pthread_self");

var _free = Module["_free"] = makeInvalidEarlyAccess("_free");

var _malloc = Module["_malloc"] = makeInvalidEarlyAccess("_malloc");

var __emscripten_proxy_main = Module["__emscripten_proxy_main"] = makeInvalidEarlyAccess("__emscripten_proxy_main");

var _emscripten_stack_get_base = makeInvalidEarlyAccess("_emscripten_stack_get_base");

var _emscripten_stack_get_end = makeInvalidEarlyAccess("_emscripten_stack_get_end");

var __emscripten_thread_init = makeInvalidEarlyAccess("__emscripten_thread_init");

var ___set_thread_state = makeInvalidEarlyAccess("___set_thread_state");

var __emscripten_thread_crashed = makeInvalidEarlyAccess("__emscripten_thread_crashed");

var _fflush = makeInvalidEarlyAccess("_fflush");

var _htonl = makeInvalidEarlyAccess("_htonl");

var _htons = makeInvalidEarlyAccess("_htons");

var _emscripten_proxy_execute_queue = makeInvalidEarlyAccess("_emscripten_proxy_execute_queue");

var _ntohs = makeInvalidEarlyAccess("_ntohs");

var _emscripten_proxy_finish = makeInvalidEarlyAccess("_emscripten_proxy_finish");

var __emscripten_run_js_on_main_thread_done = makeInvalidEarlyAccess("__emscripten_run_js_on_main_thread_done");

var __emscripten_run_js_on_main_thread = makeInvalidEarlyAccess("__emscripten_run_js_on_main_thread");

var __emscripten_thread_free_data = makeInvalidEarlyAccess("__emscripten_thread_free_data");

var __emscripten_thread_exit = makeInvalidEarlyAccess("__emscripten_thread_exit");

var __emscripten_check_mailbox = makeInvalidEarlyAccess("__emscripten_check_mailbox");

var _setThrew = makeInvalidEarlyAccess("_setThrew");

var __emscripten_tempret_set = makeInvalidEarlyAccess("__emscripten_tempret_set");

var _emscripten_stack_init = makeInvalidEarlyAccess("_emscripten_stack_init");

var _emscripten_stack_set_limits = makeInvalidEarlyAccess("_emscripten_stack_set_limits");

var _emscripten_stack_get_free = makeInvalidEarlyAccess("_emscripten_stack_get_free");

var __emscripten_stack_restore = makeInvalidEarlyAccess("__emscripten_stack_restore");

var __emscripten_stack_alloc = makeInvalidEarlyAccess("__emscripten_stack_alloc");

var _emscripten_stack_get_current = makeInvalidEarlyAccess("_emscripten_stack_get_current");

var ___cxa_increment_exception_refcount = makeInvalidEarlyAccess("___cxa_increment_exception_refcount");

var ___cxa_decrement_exception_refcount = makeInvalidEarlyAccess("___cxa_decrement_exception_refcount");

var ___get_exception_message = makeInvalidEarlyAccess("___get_exception_message");

var ___cxa_can_catch = makeInvalidEarlyAccess("___cxa_can_catch");

var ___cxa_get_exception_ptr = makeInvalidEarlyAccess("___cxa_get_exception_ptr");

var __wasmfs_opfs_record_entry = makeInvalidEarlyAccess("__wasmfs_opfs_record_entry");

var _wasmfs_flush = makeInvalidEarlyAccess("_wasmfs_flush");

var ___set_stack_limits = Module["___set_stack_limits"] = makeInvalidEarlyAccess("___set_stack_limits");

var dynCall_iii = makeInvalidEarlyAccess("dynCall_iii");

var dynCall_ii = makeInvalidEarlyAccess("dynCall_ii");

var dynCall_iiii = makeInvalidEarlyAccess("dynCall_iiii");

var dynCall_vi = makeInvalidEarlyAccess("dynCall_vi");

var dynCall_vii = makeInvalidEarlyAccess("dynCall_vii");

var dynCall_viiii = makeInvalidEarlyAccess("dynCall_viiii");

var dynCall_viii = makeInvalidEarlyAccess("dynCall_viii");

var dynCall_iiiii = makeInvalidEarlyAccess("dynCall_iiiii");

var dynCall_viiiiii = makeInvalidEarlyAccess("dynCall_viiiiii");

var dynCall_viiiiiii = makeInvalidEarlyAccess("dynCall_viiiiiii");

var dynCall_i = makeInvalidEarlyAccess("dynCall_i");

var dynCall_viiiii = makeInvalidEarlyAccess("dynCall_viiiii");

var dynCall_vijii = makeInvalidEarlyAccess("dynCall_vijii");

var dynCall_jii = makeInvalidEarlyAccess("dynCall_jii");

var dynCall_ji = makeInvalidEarlyAccess("dynCall_ji");

var dynCall_vij = makeInvalidEarlyAccess("dynCall_vij");

var dynCall_dii = makeInvalidEarlyAccess("dynCall_dii");

var dynCall_di = makeInvalidEarlyAccess("dynCall_di");

var dynCall_iiiiiiiii = makeInvalidEarlyAccess("dynCall_iiiiiiiii");

var dynCall_viiif = makeInvalidEarlyAccess("dynCall_viiif");

var dynCall_viifiii = makeInvalidEarlyAccess("dynCall_viifiii");

var dynCall_viij = makeInvalidEarlyAccess("dynCall_viij");

var dynCall_viiji = makeInvalidEarlyAccess("dynCall_viiji");

var dynCall_viijii = makeInvalidEarlyAccess("dynCall_viijii");

var dynCall_iiji = makeInvalidEarlyAccess("dynCall_iiji");

var dynCall_iij = makeInvalidEarlyAccess("dynCall_iij");

var dynCall_fi = makeInvalidEarlyAccess("dynCall_fi");

var dynCall_fiif = makeInvalidEarlyAccess("dynCall_fiif");

var dynCall_viiiiiiii = makeInvalidEarlyAccess("dynCall_viiiiiiii");

var dynCall_jiii = makeInvalidEarlyAccess("dynCall_jiii");

var dynCall_v = makeInvalidEarlyAccess("dynCall_v");

var dynCall_iidiiiii = makeInvalidEarlyAccess("dynCall_iidiiiii");

var dynCall_jiji = makeInvalidEarlyAccess("dynCall_jiji");

var dynCall_iiiij = makeInvalidEarlyAccess("dynCall_iiiij");

var _asyncify_start_unwind = makeInvalidEarlyAccess("_asyncify_start_unwind");

var _asyncify_stop_unwind = makeInvalidEarlyAccess("_asyncify_stop_unwind");

var _asyncify_start_rewind = makeInvalidEarlyAccess("_asyncify_start_rewind");

var _asyncify_stop_rewind = makeInvalidEarlyAccess("_asyncify_stop_rewind");

var __indirect_function_table = makeInvalidEarlyAccess("__indirect_function_table");

var wasmTable = makeInvalidEarlyAccess("wasmTable");

function assignWasmExports(wasmExports) {
  assert(typeof wasmExports["__main_argc_argv"] != "undefined", "missing Wasm export: __main_argc_argv");
  assert(typeof wasmExports["lucivy_add"] != "undefined", "missing Wasm export: lucivy_add");
  assert(typeof wasmExports["lucivy_add_many"] != "undefined", "missing Wasm export: lucivy_add_many");
  assert(typeof wasmExports["lucivy_apply_sharded_delta"] != "undefined", "missing Wasm export: lucivy_apply_sharded_delta");
  assert(typeof wasmExports["lucivy_close"] != "undefined", "missing Wasm export: lucivy_close");
  assert(typeof wasmExports["lucivy_commit"] != "undefined", "missing Wasm export: lucivy_commit");
  assert(typeof wasmExports["lucivy_commit_async"] != "undefined", "missing Wasm export: lucivy_commit_async");
  assert(typeof wasmExports["lucivy_commit_finish"] != "undefined", "missing Wasm export: lucivy_commit_finish");
  assert(typeof wasmExports["lucivy_commit_status_ptr"] != "undefined", "missing Wasm export: lucivy_commit_status_ptr");
  assert(typeof wasmExports["lucivy_configure"] != "undefined", "missing Wasm export: lucivy_configure");
  assert(typeof wasmExports["lucivy_create"] != "undefined", "missing Wasm export: lucivy_create");
  assert(typeof wasmExports["lucivy_destroy"] != "undefined", "missing Wasm export: lucivy_destroy");
  assert(typeof wasmExports["lucivy_drain_merges"] != "undefined", "missing Wasm export: lucivy_drain_merges");
  assert(typeof wasmExports["lucivy_dump_mermaid"] != "undefined", "missing Wasm export: lucivy_dump_mermaid");
  assert(typeof wasmExports["lucivy_dump_state"] != "undefined", "missing Wasm export: lucivy_dump_state");
  assert(typeof wasmExports["lucivy_dump_wait_graph"] != "undefined", "missing Wasm export: lucivy_dump_wait_graph");
  assert(typeof wasmExports["lucivy_dump_wait_graph_text"] != "undefined", "missing Wasm export: lucivy_dump_wait_graph_text");
  assert(typeof wasmExports["lucivy_export_sharded_delta"] != "undefined", "missing Wasm export: lucivy_export_sharded_delta");
  assert(typeof wasmExports["lucivy_export_snapshot"] != "undefined", "missing Wasm export: lucivy_export_snapshot");
  assert(typeof wasmExports["lucivy_export_stats"] != "undefined", "missing Wasm export: lucivy_export_stats");
  assert(typeof wasmExports["lucivy_import_file"] != "undefined", "missing Wasm export: lucivy_import_file");
  assert(typeof wasmExports["lucivy_import_snapshot"] != "undefined", "missing Wasm export: lucivy_import_snapshot");
  assert(typeof wasmExports["lucivy_log_ring_ptr"] != "undefined", "missing Wasm export: lucivy_log_ring_ptr");
  assert(typeof wasmExports["lucivy_log_ring_size"] != "undefined", "missing Wasm export: lucivy_log_ring_size");
  assert(typeof wasmExports["lucivy_merge_stats"] != "undefined", "missing Wasm export: lucivy_merge_stats");
  assert(typeof wasmExports["lucivy_num_docs"] != "undefined", "missing Wasm export: lucivy_num_docs");
  assert(typeof wasmExports["lucivy_open"] != "undefined", "missing Wasm export: lucivy_open");
  assert(typeof wasmExports["lucivy_open_begin"] != "undefined", "missing Wasm export: lucivy_open_begin");
  assert(typeof wasmExports["lucivy_open_finish"] != "undefined", "missing Wasm export: lucivy_open_finish");
  assert(typeof wasmExports["lucivy_query_warnings"] != "undefined", "missing Wasm export: lucivy_query_warnings");
  assert(typeof wasmExports["lucivy_read_logs"] != "undefined", "missing Wasm export: lucivy_read_logs");
  assert(typeof wasmExports["lucivy_remove"] != "undefined", "missing Wasm export: lucivy_remove");
  assert(typeof wasmExports["lucivy_schema_json"] != "undefined", "missing Wasm export: lucivy_schema_json");
  assert(typeof wasmExports["lucivy_search"] != "undefined", "missing Wasm export: lucivy_search");
  assert(typeof wasmExports["lucivy_search_filtered"] != "undefined", "missing Wasm export: lucivy_search_filtered");
  assert(typeof wasmExports["lucivy_search_with_global_stats"] != "undefined", "missing Wasm export: lucivy_search_with_global_stats");
  assert(typeof wasmExports["lucivy_shard_versions"] != "undefined", "missing Wasm export: lucivy_shard_versions");
  assert(typeof wasmExports["lucivy_test_condvar"] != "undefined", "missing Wasm export: lucivy_test_condvar");
  assert(typeof wasmExports["lucivy_test_coop"] != "undefined", "missing Wasm export: lucivy_test_coop");
  assert(typeof wasmExports["lucivy_update"] != "undefined", "missing Wasm export: lucivy_update");
  assert(typeof wasmExports["_emscripten_tls_init"] != "undefined", "missing Wasm export: _emscripten_tls_init");
  assert(typeof wasmExports["pthread_self"] != "undefined", "missing Wasm export: pthread_self");
  assert(typeof wasmExports["free"] != "undefined", "missing Wasm export: free");
  assert(typeof wasmExports["malloc"] != "undefined", "missing Wasm export: malloc");
  assert(typeof wasmExports["_emscripten_proxy_main"] != "undefined", "missing Wasm export: _emscripten_proxy_main");
  assert(typeof wasmExports["emscripten_stack_get_base"] != "undefined", "missing Wasm export: emscripten_stack_get_base");
  assert(typeof wasmExports["emscripten_stack_get_end"] != "undefined", "missing Wasm export: emscripten_stack_get_end");
  assert(typeof wasmExports["_emscripten_thread_init"] != "undefined", "missing Wasm export: _emscripten_thread_init");
  assert(typeof wasmExports["__set_thread_state"] != "undefined", "missing Wasm export: __set_thread_state");
  assert(typeof wasmExports["_emscripten_thread_crashed"] != "undefined", "missing Wasm export: _emscripten_thread_crashed");
  assert(typeof wasmExports["fflush"] != "undefined", "missing Wasm export: fflush");
  assert(typeof wasmExports["htonl"] != "undefined", "missing Wasm export: htonl");
  assert(typeof wasmExports["htons"] != "undefined", "missing Wasm export: htons");
  assert(typeof wasmExports["emscripten_proxy_execute_queue"] != "undefined", "missing Wasm export: emscripten_proxy_execute_queue");
  assert(typeof wasmExports["ntohs"] != "undefined", "missing Wasm export: ntohs");
  assert(typeof wasmExports["emscripten_proxy_finish"] != "undefined", "missing Wasm export: emscripten_proxy_finish");
  assert(typeof wasmExports["_emscripten_run_js_on_main_thread_done"] != "undefined", "missing Wasm export: _emscripten_run_js_on_main_thread_done");
  assert(typeof wasmExports["_emscripten_run_js_on_main_thread"] != "undefined", "missing Wasm export: _emscripten_run_js_on_main_thread");
  assert(typeof wasmExports["_emscripten_thread_free_data"] != "undefined", "missing Wasm export: _emscripten_thread_free_data");
  assert(typeof wasmExports["_emscripten_thread_exit"] != "undefined", "missing Wasm export: _emscripten_thread_exit");
  assert(typeof wasmExports["_emscripten_check_mailbox"] != "undefined", "missing Wasm export: _emscripten_check_mailbox");
  assert(typeof wasmExports["setThrew"] != "undefined", "missing Wasm export: setThrew");
  assert(typeof wasmExports["_emscripten_tempret_set"] != "undefined", "missing Wasm export: _emscripten_tempret_set");
  assert(typeof wasmExports["emscripten_stack_init"] != "undefined", "missing Wasm export: emscripten_stack_init");
  assert(typeof wasmExports["emscripten_stack_set_limits"] != "undefined", "missing Wasm export: emscripten_stack_set_limits");
  assert(typeof wasmExports["emscripten_stack_get_free"] != "undefined", "missing Wasm export: emscripten_stack_get_free");
  assert(typeof wasmExports["_emscripten_stack_restore"] != "undefined", "missing Wasm export: _emscripten_stack_restore");
  assert(typeof wasmExports["_emscripten_stack_alloc"] != "undefined", "missing Wasm export: _emscripten_stack_alloc");
  assert(typeof wasmExports["emscripten_stack_get_current"] != "undefined", "missing Wasm export: emscripten_stack_get_current");
  assert(typeof wasmExports["__cxa_increment_exception_refcount"] != "undefined", "missing Wasm export: __cxa_increment_exception_refcount");
  assert(typeof wasmExports["__cxa_decrement_exception_refcount"] != "undefined", "missing Wasm export: __cxa_decrement_exception_refcount");
  assert(typeof wasmExports["__get_exception_message"] != "undefined", "missing Wasm export: __get_exception_message");
  assert(typeof wasmExports["__cxa_can_catch"] != "undefined", "missing Wasm export: __cxa_can_catch");
  assert(typeof wasmExports["__cxa_get_exception_ptr"] != "undefined", "missing Wasm export: __cxa_get_exception_ptr");
  assert(typeof wasmExports["_wasmfs_opfs_record_entry"] != "undefined", "missing Wasm export: _wasmfs_opfs_record_entry");
  assert(typeof wasmExports["wasmfs_flush"] != "undefined", "missing Wasm export: wasmfs_flush");
  assert(typeof wasmExports["__set_stack_limits"] != "undefined", "missing Wasm export: __set_stack_limits");
  assert(typeof wasmExports["dynCall_iii"] != "undefined", "missing Wasm export: dynCall_iii");
  assert(typeof wasmExports["dynCall_ii"] != "undefined", "missing Wasm export: dynCall_ii");
  assert(typeof wasmExports["dynCall_iiii"] != "undefined", "missing Wasm export: dynCall_iiii");
  assert(typeof wasmExports["dynCall_vi"] != "undefined", "missing Wasm export: dynCall_vi");
  assert(typeof wasmExports["dynCall_vii"] != "undefined", "missing Wasm export: dynCall_vii");
  assert(typeof wasmExports["dynCall_viiii"] != "undefined", "missing Wasm export: dynCall_viiii");
  assert(typeof wasmExports["dynCall_viii"] != "undefined", "missing Wasm export: dynCall_viii");
  assert(typeof wasmExports["dynCall_iiiii"] != "undefined", "missing Wasm export: dynCall_iiiii");
  assert(typeof wasmExports["dynCall_viiiiii"] != "undefined", "missing Wasm export: dynCall_viiiiii");
  assert(typeof wasmExports["dynCall_viiiiiii"] != "undefined", "missing Wasm export: dynCall_viiiiiii");
  assert(typeof wasmExports["dynCall_i"] != "undefined", "missing Wasm export: dynCall_i");
  assert(typeof wasmExports["dynCall_viiiii"] != "undefined", "missing Wasm export: dynCall_viiiii");
  assert(typeof wasmExports["dynCall_vijii"] != "undefined", "missing Wasm export: dynCall_vijii");
  assert(typeof wasmExports["dynCall_jii"] != "undefined", "missing Wasm export: dynCall_jii");
  assert(typeof wasmExports["dynCall_ji"] != "undefined", "missing Wasm export: dynCall_ji");
  assert(typeof wasmExports["dynCall_vij"] != "undefined", "missing Wasm export: dynCall_vij");
  assert(typeof wasmExports["dynCall_dii"] != "undefined", "missing Wasm export: dynCall_dii");
  assert(typeof wasmExports["dynCall_di"] != "undefined", "missing Wasm export: dynCall_di");
  assert(typeof wasmExports["dynCall_iiiiiiiii"] != "undefined", "missing Wasm export: dynCall_iiiiiiiii");
  assert(typeof wasmExports["dynCall_viiif"] != "undefined", "missing Wasm export: dynCall_viiif");
  assert(typeof wasmExports["dynCall_viifiii"] != "undefined", "missing Wasm export: dynCall_viifiii");
  assert(typeof wasmExports["dynCall_viij"] != "undefined", "missing Wasm export: dynCall_viij");
  assert(typeof wasmExports["dynCall_viiji"] != "undefined", "missing Wasm export: dynCall_viiji");
  assert(typeof wasmExports["dynCall_viijii"] != "undefined", "missing Wasm export: dynCall_viijii");
  assert(typeof wasmExports["dynCall_iiji"] != "undefined", "missing Wasm export: dynCall_iiji");
  assert(typeof wasmExports["dynCall_iij"] != "undefined", "missing Wasm export: dynCall_iij");
  assert(typeof wasmExports["dynCall_fi"] != "undefined", "missing Wasm export: dynCall_fi");
  assert(typeof wasmExports["dynCall_fiif"] != "undefined", "missing Wasm export: dynCall_fiif");
  assert(typeof wasmExports["dynCall_viiiiiiii"] != "undefined", "missing Wasm export: dynCall_viiiiiiii");
  assert(typeof wasmExports["dynCall_jiii"] != "undefined", "missing Wasm export: dynCall_jiii");
  assert(typeof wasmExports["dynCall_v"] != "undefined", "missing Wasm export: dynCall_v");
  assert(typeof wasmExports["dynCall_iidiiiii"] != "undefined", "missing Wasm export: dynCall_iidiiiii");
  assert(typeof wasmExports["dynCall_jiji"] != "undefined", "missing Wasm export: dynCall_jiji");
  assert(typeof wasmExports["dynCall_iiiij"] != "undefined", "missing Wasm export: dynCall_iiiij");
  assert(typeof wasmExports["asyncify_start_unwind"] != "undefined", "missing Wasm export: asyncify_start_unwind");
  assert(typeof wasmExports["asyncify_stop_unwind"] != "undefined", "missing Wasm export: asyncify_stop_unwind");
  assert(typeof wasmExports["asyncify_start_rewind"] != "undefined", "missing Wasm export: asyncify_start_rewind");
  assert(typeof wasmExports["asyncify_stop_rewind"] != "undefined", "missing Wasm export: asyncify_stop_rewind");
  assert(typeof wasmExports["__indirect_function_table"] != "undefined", "missing Wasm export: __indirect_function_table");
  _main = Module["_main"] = createExportWrapper("__main_argc_argv", wasmExports["__main_argc_argv"], 2);
  _lucivy_add = Module["_lucivy_add"] = createExportWrapper("lucivy_add", wasmExports["lucivy_add"], 3);
  _lucivy_add_many = Module["_lucivy_add_many"] = createExportWrapper("lucivy_add_many", wasmExports["lucivy_add_many"], 2);
  _lucivy_apply_sharded_delta = Module["_lucivy_apply_sharded_delta"] = createExportWrapper("lucivy_apply_sharded_delta", wasmExports["lucivy_apply_sharded_delta"], 3);
  _lucivy_close = Module["_lucivy_close"] = createExportWrapper("lucivy_close", wasmExports["lucivy_close"], 1);
  _lucivy_commit = Module["_lucivy_commit"] = createExportWrapper("lucivy_commit", wasmExports["lucivy_commit"], 1);
  _lucivy_commit_async = Module["_lucivy_commit_async"] = createExportWrapper("lucivy_commit_async", wasmExports["lucivy_commit_async"], 1);
  _lucivy_commit_finish = Module["_lucivy_commit_finish"] = createExportWrapper("lucivy_commit_finish", wasmExports["lucivy_commit_finish"], 0);
  _lucivy_commit_status_ptr = Module["_lucivy_commit_status_ptr"] = createExportWrapper("lucivy_commit_status_ptr", wasmExports["lucivy_commit_status_ptr"], 0);
  _lucivy_configure = Module["_lucivy_configure"] = createExportWrapper("lucivy_configure", wasmExports["lucivy_configure"], 2);
  _lucivy_create = Module["_lucivy_create"] = createExportWrapper("lucivy_create", wasmExports["lucivy_create"], 2);
  _lucivy_destroy = Module["_lucivy_destroy"] = createExportWrapper("lucivy_destroy", wasmExports["lucivy_destroy"], 1);
  _lucivy_drain_merges = Module["_lucivy_drain_merges"] = createExportWrapper("lucivy_drain_merges", wasmExports["lucivy_drain_merges"], 1);
  _lucivy_dump_mermaid = Module["_lucivy_dump_mermaid"] = createExportWrapper("lucivy_dump_mermaid", wasmExports["lucivy_dump_mermaid"], 0);
  _lucivy_dump_state = Module["_lucivy_dump_state"] = createExportWrapper("lucivy_dump_state", wasmExports["lucivy_dump_state"], 0);
  _lucivy_dump_wait_graph = Module["_lucivy_dump_wait_graph"] = createExportWrapper("lucivy_dump_wait_graph", wasmExports["lucivy_dump_wait_graph"], 0);
  _lucivy_dump_wait_graph_text = Module["_lucivy_dump_wait_graph_text"] = createExportWrapper("lucivy_dump_wait_graph_text", wasmExports["lucivy_dump_wait_graph_text"], 0);
  _lucivy_export_sharded_delta = Module["_lucivy_export_sharded_delta"] = createExportWrapper("lucivy_export_sharded_delta", wasmExports["lucivy_export_sharded_delta"], 3);
  _lucivy_export_snapshot = Module["_lucivy_export_snapshot"] = createExportWrapper("lucivy_export_snapshot", wasmExports["lucivy_export_snapshot"], 2);
  _lucivy_export_stats = Module["_lucivy_export_stats"] = createExportWrapper("lucivy_export_stats", wasmExports["lucivy_export_stats"], 2);
  _lucivy_import_file = Module["_lucivy_import_file"] = createExportWrapper("lucivy_import_file", wasmExports["lucivy_import_file"], 4);
  _lucivy_import_snapshot = Module["_lucivy_import_snapshot"] = createExportWrapper("lucivy_import_snapshot", wasmExports["lucivy_import_snapshot"], 3);
  _lucivy_log_ring_ptr = Module["_lucivy_log_ring_ptr"] = createExportWrapper("lucivy_log_ring_ptr", wasmExports["lucivy_log_ring_ptr"], 0);
  _lucivy_log_ring_size = Module["_lucivy_log_ring_size"] = createExportWrapper("lucivy_log_ring_size", wasmExports["lucivy_log_ring_size"], 0);
  _lucivy_merge_stats = Module["_lucivy_merge_stats"] = createExportWrapper("lucivy_merge_stats", wasmExports["lucivy_merge_stats"], 1);
  _lucivy_num_docs = Module["_lucivy_num_docs"] = createExportWrapper("lucivy_num_docs", wasmExports["lucivy_num_docs"], 1);
  _lucivy_open = Module["_lucivy_open"] = createExportWrapper("lucivy_open", wasmExports["lucivy_open"], 1);
  _lucivy_open_begin = Module["_lucivy_open_begin"] = createExportWrapper("lucivy_open_begin", wasmExports["lucivy_open_begin"], 1);
  _lucivy_open_finish = Module["_lucivy_open_finish"] = createExportWrapper("lucivy_open_finish", wasmExports["lucivy_open_finish"], 1);
  _lucivy_query_warnings = Module["_lucivy_query_warnings"] = createExportWrapper("lucivy_query_warnings", wasmExports["lucivy_query_warnings"], 2);
  _lucivy_read_logs = Module["_lucivy_read_logs"] = createExportWrapper("lucivy_read_logs", wasmExports["lucivy_read_logs"], 0);
  _lucivy_remove = Module["_lucivy_remove"] = createExportWrapper("lucivy_remove", wasmExports["lucivy_remove"], 2);
  _lucivy_schema_json = Module["_lucivy_schema_json"] = createExportWrapper("lucivy_schema_json", wasmExports["lucivy_schema_json"], 1);
  _lucivy_search = Module["_lucivy_search"] = createExportWrapper("lucivy_search", wasmExports["lucivy_search"], 5);
  _lucivy_search_filtered = Module["_lucivy_search_filtered"] = createExportWrapper("lucivy_search_filtered", wasmExports["lucivy_search_filtered"], 7);
  _lucivy_search_with_global_stats = Module["_lucivy_search_with_global_stats"] = createExportWrapper("lucivy_search_with_global_stats", wasmExports["lucivy_search_with_global_stats"], 5);
  _lucivy_shard_versions = Module["_lucivy_shard_versions"] = createExportWrapper("lucivy_shard_versions", wasmExports["lucivy_shard_versions"], 1);
  _lucivy_test_condvar = Module["_lucivy_test_condvar"] = createExportWrapper("lucivy_test_condvar", wasmExports["lucivy_test_condvar"], 0);
  _lucivy_test_coop = Module["_lucivy_test_coop"] = createExportWrapper("lucivy_test_coop", wasmExports["lucivy_test_coop"], 0);
  _lucivy_update = Module["_lucivy_update"] = createExportWrapper("lucivy_update", wasmExports["lucivy_update"], 3);
  __emscripten_tls_init = createExportWrapper("_emscripten_tls_init", wasmExports["_emscripten_tls_init"], 0);
  _pthread_self = wasmExports["pthread_self"];
  _free = Module["_free"] = createExportWrapper("free", wasmExports["free"], 1);
  _malloc = Module["_malloc"] = createExportWrapper("malloc", wasmExports["malloc"], 1);
  __emscripten_proxy_main = Module["__emscripten_proxy_main"] = createExportWrapper("_emscripten_proxy_main", wasmExports["_emscripten_proxy_main"], 2);
  _emscripten_stack_get_base = wasmExports["emscripten_stack_get_base"];
  _emscripten_stack_get_end = wasmExports["emscripten_stack_get_end"];
  __emscripten_thread_init = createExportWrapper("_emscripten_thread_init", wasmExports["_emscripten_thread_init"], 6);
  ___set_thread_state = createExportWrapper("__set_thread_state", wasmExports["__set_thread_state"], 4);
  __emscripten_thread_crashed = createExportWrapper("_emscripten_thread_crashed", wasmExports["_emscripten_thread_crashed"], 0);
  _fflush = createExportWrapper("fflush", wasmExports["fflush"], 1);
  _htonl = createExportWrapper("htonl", wasmExports["htonl"], 1);
  _htons = createExportWrapper("htons", wasmExports["htons"], 1);
  _emscripten_proxy_execute_queue = createExportWrapper("emscripten_proxy_execute_queue", wasmExports["emscripten_proxy_execute_queue"], 1);
  _ntohs = createExportWrapper("ntohs", wasmExports["ntohs"], 1);
  _emscripten_proxy_finish = createExportWrapper("emscripten_proxy_finish", wasmExports["emscripten_proxy_finish"], 1);
  __emscripten_run_js_on_main_thread_done = createExportWrapper("_emscripten_run_js_on_main_thread_done", wasmExports["_emscripten_run_js_on_main_thread_done"], 3);
  __emscripten_run_js_on_main_thread = createExportWrapper("_emscripten_run_js_on_main_thread", wasmExports["_emscripten_run_js_on_main_thread"], 5);
  __emscripten_thread_free_data = createExportWrapper("_emscripten_thread_free_data", wasmExports["_emscripten_thread_free_data"], 1);
  __emscripten_thread_exit = createExportWrapper("_emscripten_thread_exit", wasmExports["_emscripten_thread_exit"], 1);
  __emscripten_check_mailbox = createExportWrapper("_emscripten_check_mailbox", wasmExports["_emscripten_check_mailbox"], 0);
  _setThrew = createExportWrapper("setThrew", wasmExports["setThrew"], 2);
  __emscripten_tempret_set = createExportWrapper("_emscripten_tempret_set", wasmExports["_emscripten_tempret_set"], 1);
  _emscripten_stack_init = wasmExports["emscripten_stack_init"];
  _emscripten_stack_set_limits = wasmExports["emscripten_stack_set_limits"];
  _emscripten_stack_get_free = wasmExports["emscripten_stack_get_free"];
  __emscripten_stack_restore = wasmExports["_emscripten_stack_restore"];
  __emscripten_stack_alloc = wasmExports["_emscripten_stack_alloc"];
  _emscripten_stack_get_current = wasmExports["emscripten_stack_get_current"];
  ___cxa_increment_exception_refcount = createExportWrapper("__cxa_increment_exception_refcount", wasmExports["__cxa_increment_exception_refcount"], 1);
  ___cxa_decrement_exception_refcount = createExportWrapper("__cxa_decrement_exception_refcount", wasmExports["__cxa_decrement_exception_refcount"], 1);
  ___get_exception_message = createExportWrapper("__get_exception_message", wasmExports["__get_exception_message"], 3);
  ___cxa_can_catch = createExportWrapper("__cxa_can_catch", wasmExports["__cxa_can_catch"], 3);
  ___cxa_get_exception_ptr = createExportWrapper("__cxa_get_exception_ptr", wasmExports["__cxa_get_exception_ptr"], 1);
  __wasmfs_opfs_record_entry = createExportWrapper("_wasmfs_opfs_record_entry", wasmExports["_wasmfs_opfs_record_entry"], 3);
  _wasmfs_flush = createExportWrapper("wasmfs_flush", wasmExports["wasmfs_flush"], 0);
  ___set_stack_limits = Module["___set_stack_limits"] = createExportWrapper("__set_stack_limits", wasmExports["__set_stack_limits"], 2);
  dynCall_iii = dynCalls["iii"] = createExportWrapper("dynCall_iii", wasmExports["dynCall_iii"], 3);
  dynCall_ii = dynCalls["ii"] = createExportWrapper("dynCall_ii", wasmExports["dynCall_ii"], 2);
  dynCall_iiii = dynCalls["iiii"] = createExportWrapper("dynCall_iiii", wasmExports["dynCall_iiii"], 4);
  dynCall_vi = dynCalls["vi"] = createExportWrapper("dynCall_vi", wasmExports["dynCall_vi"], 2);
  dynCall_vii = dynCalls["vii"] = createExportWrapper("dynCall_vii", wasmExports["dynCall_vii"], 3);
  dynCall_viiii = dynCalls["viiii"] = createExportWrapper("dynCall_viiii", wasmExports["dynCall_viiii"], 5);
  dynCall_viii = dynCalls["viii"] = createExportWrapper("dynCall_viii", wasmExports["dynCall_viii"], 4);
  dynCall_iiiii = dynCalls["iiiii"] = createExportWrapper("dynCall_iiiii", wasmExports["dynCall_iiiii"], 5);
  dynCall_viiiiii = dynCalls["viiiiii"] = createExportWrapper("dynCall_viiiiii", wasmExports["dynCall_viiiiii"], 7);
  dynCall_viiiiiii = dynCalls["viiiiiii"] = createExportWrapper("dynCall_viiiiiii", wasmExports["dynCall_viiiiiii"], 8);
  dynCall_i = dynCalls["i"] = createExportWrapper("dynCall_i", wasmExports["dynCall_i"], 1);
  dynCall_viiiii = dynCalls["viiiii"] = createExportWrapper("dynCall_viiiii", wasmExports["dynCall_viiiii"], 6);
  dynCall_vijii = dynCalls["vijii"] = createExportWrapper("dynCall_vijii", wasmExports["dynCall_vijii"], 5);
  dynCall_jii = dynCalls["jii"] = createExportWrapper("dynCall_jii", wasmExports["dynCall_jii"], 3);
  dynCall_ji = dynCalls["ji"] = createExportWrapper("dynCall_ji", wasmExports["dynCall_ji"], 2);
  dynCall_vij = dynCalls["vij"] = createExportWrapper("dynCall_vij", wasmExports["dynCall_vij"], 3);
  dynCall_dii = dynCalls["dii"] = createExportWrapper("dynCall_dii", wasmExports["dynCall_dii"], 3);
  dynCall_di = dynCalls["di"] = createExportWrapper("dynCall_di", wasmExports["dynCall_di"], 2);
  dynCall_iiiiiiiii = dynCalls["iiiiiiiii"] = createExportWrapper("dynCall_iiiiiiiii", wasmExports["dynCall_iiiiiiiii"], 9);
  dynCall_viiif = dynCalls["viiif"] = createExportWrapper("dynCall_viiif", wasmExports["dynCall_viiif"], 5);
  dynCall_viifiii = dynCalls["viifiii"] = createExportWrapper("dynCall_viifiii", wasmExports["dynCall_viifiii"], 7);
  dynCall_viij = dynCalls["viij"] = createExportWrapper("dynCall_viij", wasmExports["dynCall_viij"], 4);
  dynCall_viiji = dynCalls["viiji"] = createExportWrapper("dynCall_viiji", wasmExports["dynCall_viiji"], 5);
  dynCall_viijii = dynCalls["viijii"] = createExportWrapper("dynCall_viijii", wasmExports["dynCall_viijii"], 6);
  dynCall_iiji = dynCalls["iiji"] = createExportWrapper("dynCall_iiji", wasmExports["dynCall_iiji"], 4);
  dynCall_iij = dynCalls["iij"] = createExportWrapper("dynCall_iij", wasmExports["dynCall_iij"], 3);
  dynCall_fi = dynCalls["fi"] = createExportWrapper("dynCall_fi", wasmExports["dynCall_fi"], 2);
  dynCall_fiif = dynCalls["fiif"] = createExportWrapper("dynCall_fiif", wasmExports["dynCall_fiif"], 4);
  dynCall_viiiiiiii = dynCalls["viiiiiiii"] = createExportWrapper("dynCall_viiiiiiii", wasmExports["dynCall_viiiiiiii"], 9);
  dynCall_jiii = dynCalls["jiii"] = createExportWrapper("dynCall_jiii", wasmExports["dynCall_jiii"], 4);
  dynCall_v = dynCalls["v"] = createExportWrapper("dynCall_v", wasmExports["dynCall_v"], 1);
  dynCall_iidiiiii = dynCalls["iidiiiii"] = createExportWrapper("dynCall_iidiiiii", wasmExports["dynCall_iidiiiii"], 8);
  dynCall_jiji = dynCalls["jiji"] = createExportWrapper("dynCall_jiji", wasmExports["dynCall_jiji"], 4);
  dynCall_iiiij = dynCalls["iiiij"] = createExportWrapper("dynCall_iiiij", wasmExports["dynCall_iiiij"], 5);
  _asyncify_start_unwind = createExportWrapper("asyncify_start_unwind", wasmExports["asyncify_start_unwind"], 1);
  _asyncify_stop_unwind = createExportWrapper("asyncify_stop_unwind", wasmExports["asyncify_stop_unwind"], 0);
  _asyncify_start_rewind = createExportWrapper("asyncify_start_rewind", wasmExports["asyncify_start_rewind"], 1);
  _asyncify_stop_rewind = createExportWrapper("asyncify_stop_rewind", wasmExports["asyncify_stop_rewind"], 0);
  __indirect_function_table = wasmTable = wasmExports["__indirect_function_table"];
}

var wasmImports;

function assignWasmImports() {
  wasmImports = {
    /** @export */ __assert_fail: ___assert_fail,
    /** @export */ __call_sighandler: ___call_sighandler,
    /** @export */ __cxa_begin_catch: ___cxa_begin_catch,
    /** @export */ __cxa_find_matching_catch_2: ___cxa_find_matching_catch_2,
    /** @export */ __cxa_find_matching_catch_3: ___cxa_find_matching_catch_3,
    /** @export */ __cxa_throw: ___cxa_throw,
    /** @export */ __handle_stack_overflow: ___handle_stack_overflow,
    /** @export */ __pthread_create_js: ___pthread_create_js,
    /** @export */ __resumeException: ___resumeException,
    /** @export */ _abort_js: __abort_js,
    /** @export */ _emscripten_init_main_thread_js: __emscripten_init_main_thread_js,
    /** @export */ _emscripten_notify_mailbox_postmessage: __emscripten_notify_mailbox_postmessage,
    /** @export */ _emscripten_receive_on_main_thread_js: __emscripten_receive_on_main_thread_js,
    /** @export */ _emscripten_runtime_keepalive_clear: __emscripten_runtime_keepalive_clear,
    /** @export */ _emscripten_thread_cleanup: __emscripten_thread_cleanup,
    /** @export */ _emscripten_thread_mailbox_await: __emscripten_thread_mailbox_await,
    /** @export */ _emscripten_thread_set_strongref: __emscripten_thread_set_strongref,
    /** @export */ _wasmfs_copy_preloaded_file_data: __wasmfs_copy_preloaded_file_data,
    /** @export */ _wasmfs_get_num_preloaded_dirs: __wasmfs_get_num_preloaded_dirs,
    /** @export */ _wasmfs_get_num_preloaded_files: __wasmfs_get_num_preloaded_files,
    /** @export */ _wasmfs_get_preloaded_child_path: __wasmfs_get_preloaded_child_path,
    /** @export */ _wasmfs_get_preloaded_file_mode: __wasmfs_get_preloaded_file_mode,
    /** @export */ _wasmfs_get_preloaded_file_size: __wasmfs_get_preloaded_file_size,
    /** @export */ _wasmfs_get_preloaded_parent_path: __wasmfs_get_preloaded_parent_path,
    /** @export */ _wasmfs_get_preloaded_path_name: __wasmfs_get_preloaded_path_name,
    /** @export */ _wasmfs_opfs_close_access: __wasmfs_opfs_close_access,
    /** @export */ _wasmfs_opfs_close_blob: __wasmfs_opfs_close_blob,
    /** @export */ _wasmfs_opfs_flush_access: __wasmfs_opfs_flush_access,
    /** @export */ _wasmfs_opfs_free_directory: __wasmfs_opfs_free_directory,
    /** @export */ _wasmfs_opfs_free_file: __wasmfs_opfs_free_file,
    /** @export */ _wasmfs_opfs_get_child: __wasmfs_opfs_get_child,
    /** @export */ _wasmfs_opfs_get_entries: __wasmfs_opfs_get_entries,
    /** @export */ _wasmfs_opfs_get_size_access: __wasmfs_opfs_get_size_access,
    /** @export */ _wasmfs_opfs_get_size_blob: __wasmfs_opfs_get_size_blob,
    /** @export */ _wasmfs_opfs_get_size_file: __wasmfs_opfs_get_size_file,
    /** @export */ _wasmfs_opfs_init_root_directory: __wasmfs_opfs_init_root_directory,
    /** @export */ _wasmfs_opfs_insert_directory: __wasmfs_opfs_insert_directory,
    /** @export */ _wasmfs_opfs_insert_file: __wasmfs_opfs_insert_file,
    /** @export */ _wasmfs_opfs_move_file: __wasmfs_opfs_move_file,
    /** @export */ _wasmfs_opfs_open_access: __wasmfs_opfs_open_access,
    /** @export */ _wasmfs_opfs_open_blob: __wasmfs_opfs_open_blob,
    /** @export */ _wasmfs_opfs_read_access: __wasmfs_opfs_read_access,
    /** @export */ _wasmfs_opfs_read_blob: __wasmfs_opfs_read_blob,
    /** @export */ _wasmfs_opfs_remove_child: __wasmfs_opfs_remove_child,
    /** @export */ _wasmfs_opfs_set_size_access: __wasmfs_opfs_set_size_access,
    /** @export */ _wasmfs_opfs_set_size_file: __wasmfs_opfs_set_size_file,
    /** @export */ _wasmfs_opfs_write_access: __wasmfs_opfs_write_access,
    /** @export */ _wasmfs_stdin_get_char: __wasmfs_stdin_get_char,
    /** @export */ _wasmfs_thread_utils_heartbeat: __wasmfs_thread_utils_heartbeat,
    /** @export */ clock_time_get: _clock_time_get,
    /** @export */ emscripten_check_blocking_allowed: _emscripten_check_blocking_allowed,
    /** @export */ emscripten_date_now: _emscripten_date_now,
    /** @export */ emscripten_err: _emscripten_err,
    /** @export */ emscripten_exit_with_live_runtime: _emscripten_exit_with_live_runtime,
    /** @export */ emscripten_get_callstack: _emscripten_get_callstack,
    /** @export */ emscripten_get_heap_max: _emscripten_get_heap_max,
    /** @export */ emscripten_get_now: _emscripten_get_now,
    /** @export */ emscripten_has_asyncify: _emscripten_has_asyncify,
    /** @export */ emscripten_num_logical_cores: _emscripten_num_logical_cores,
    /** @export */ emscripten_out: _emscripten_out,
    /** @export */ emscripten_resize_heap: _emscripten_resize_heap,
    /** @export */ emscripten_runtime_keepalive_check: _emscripten_runtime_keepalive_check,
    /** @export */ emscripten_unwind_to_js_event_loop: _emscripten_unwind_to_js_event_loop,
    /** @export */ environ_get: _environ_get,
    /** @export */ environ_sizes_get: _environ_sizes_get,
    /** @export */ exit: _exit,
    /** @export */ invoke_ii,
    /** @export */ invoke_iii,
    /** @export */ invoke_iiii,
    /** @export */ invoke_v,
    /** @export */ invoke_vi,
    /** @export */ invoke_vii,
    /** @export */ invoke_viii,
    /** @export */ invoke_viiii,
    /** @export */ memory: wasmMemory,
    /** @export */ proc_exit: _proc_exit,
    /** @export */ random_get: _random_get
  };
}

function invoke_v(index) {
  var sp = stackSave();
  try {
    dynCall_v(index);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

function invoke_vii(index, a1, a2) {
  var sp = stackSave();
  try {
    dynCall_vii(index, a1, a2);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

function invoke_ii(index, a1) {
  var sp = stackSave();
  try {
    return dynCall_ii(index, a1);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

function invoke_vi(index, a1) {
  var sp = stackSave();
  try {
    dynCall_vi(index, a1);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

function invoke_viiii(index, a1, a2, a3, a4) {
  var sp = stackSave();
  try {
    dynCall_viiii(index, a1, a2, a3, a4);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

function invoke_iii(index, a1, a2) {
  var sp = stackSave();
  try {
    return dynCall_iii(index, a1, a2);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

function invoke_viii(index, a1, a2, a3) {
  var sp = stackSave();
  try {
    dynCall_viii(index, a1, a2, a3);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

function invoke_iiii(index, a1, a2, a3) {
  var sp = stackSave();
  try {
    return dynCall_iiii(index, a1, a2, a3);
  } catch (e) {
    stackRestore(sp);
    if (!(e instanceof EmscriptenEH)) throw e;
    _setThrew(1, 0);
  }
}

// Argument name here must shadow the `wasmExports` global so
// that it is recognised by metadce and minify-import-export-names
// passes.
function applySignatureConversions(wasmExports) {
  // First, make a copy of the incoming exports object
  wasmExports = Object.assign({}, wasmExports);
  var makeWrapper_p = f => () => f() >>> 0;
  var makeWrapper_pp = f => a0 => f(a0) >>> 0;
  wasmExports["_emscripten_tls_init"] = makeWrapper_p(wasmExports["_emscripten_tls_init"]);
  wasmExports["pthread_self"] = makeWrapper_p(wasmExports["pthread_self"]);
  wasmExports["malloc"] = makeWrapper_pp(wasmExports["malloc"]);
  wasmExports["emscripten_stack_get_base"] = makeWrapper_p(wasmExports["emscripten_stack_get_base"]);
  wasmExports["emscripten_stack_get_end"] = makeWrapper_p(wasmExports["emscripten_stack_get_end"]);
  wasmExports["emscripten_stack_get_free"] = makeWrapper_p(wasmExports["emscripten_stack_get_free"]);
  wasmExports["_emscripten_stack_alloc"] = makeWrapper_pp(wasmExports["_emscripten_stack_alloc"]);
  wasmExports["emscripten_stack_get_current"] = makeWrapper_p(wasmExports["emscripten_stack_get_current"]);
  wasmExports["__cxa_get_exception_ptr"] = makeWrapper_pp(wasmExports["__cxa_get_exception_ptr"]);
  return wasmExports;
}

// include: postamble.js
// === Auto-generated postamble setup entry stuff ===
var calledRun;

function callMain(args = []) {
  assert(runDependencies == 0, 'cannot call main when async dependencies remain! (listen on Module["onRuntimeInitialized"])');
  assert(typeof onPreRuns === "undefined" || onPreRuns.length == 0, "cannot call main when preRun functions remain to be called");
  var entryFunction = __emscripten_proxy_main;
  // With PROXY_TO_PTHREAD make sure we keep the runtime alive until the
  // proxied main calls exit (see exitOnMainThread() for where Pop is called).
  runtimeKeepalivePush();
  args.unshift(thisProgram);
  var argc = args.length;
  var argv = stackAlloc((argc + 1) * 4);
  var argv_ptr = argv;
  for (var arg of args) {
    (growMemViews(), HEAPU32)[((argv_ptr) >>> 2) >>> 0] = stringToUTF8OnStack(arg);
    argv_ptr += 4;
  }
  (growMemViews(), HEAPU32)[((argv_ptr) >>> 2) >>> 0] = 0;
  try {
    var ret = entryFunction(argc, argv);
    // if we're not running an evented main loop, it's time to exit
    exitJS(ret, /* implicit = */ true);
    return ret;
  } catch (e) {
    return handleException(e);
  }
}

function stackCheckInit() {
  // This is normally called automatically during __wasm_call_ctors but need to
  // get these values before even running any of the ctors so we call it redundantly
  // here.
  // See $establishStackSpace for the equivalent code that runs on a thread
  assert(!ENVIRONMENT_IS_PTHREAD);
  _emscripten_stack_init();
  // TODO(sbc): Move writeStackCookie to native to to avoid this.
  writeStackCookie();
}

async function run(args = programArgs) {
  assert(!calledRun);
  calledRun = true;
  if ((ENVIRONMENT_IS_PTHREAD)) {
    initRuntime();
    return;
  }
  stackCheckInit();
  preRun();
  if (runDependencies) {
    await resolveRunDependencies();
  }
  var setStatus = Module["setStatus"];
  if (setStatus) {
    setStatus("Running...");
    // Yield to the event loop to allow the browser to paint "Running..."
    await new Promise(resolve => setTimeout(resolve, 1));
    // Then we want to clear the status text, but only after the rest of this function runs.
    setTimeout(setStatus, 1, "");
  }
  if (ABORT) return;
  initRuntime();
  // No ATMAINS hooks
  Module["onRuntimeInitialized"]?.();
  consumedModuleProp("onRuntimeInitialized");
  var noInitialRun = Module["noInitialRun"] || false;
  if (!noInitialRun) callMain(args);
  postRun();
}

function checkUnflushedContent() {
  // Compiler settings do not allow exiting the runtime, so flushing
  // the streams is not possible. but in ASSERTIONS mode we check
  // if there was something to flush, and if so tell the user they
  // should request that the runtime be exitable.
  // Normally we would not even include flush() at all, but in ASSERTIONS
  // builds we do so just for this check, and here we see if there is any
  // content to flush, that is, we check if there would have been
  // something a non-ASSERTIONS build would have not seen.
  // How we flush the streams depends on whether we are in SYSCALLS_REQUIRE_FILESYSTEM=0
  // mode (which has its own special function for this; otherwise, all
  // the code is inside libc)
  var oldOut = out;
  var oldErr = err;
  var has = false;
  out = err = x => {
    has = true;
  };
  try {
    // it doesn't matter if it fails
    // In WasmFS we must also flush the WasmFS internal buffers, for this check
    // to work.
    _wasmfs_flush();
  } catch (e) {}
  out = oldOut;
  err = oldErr;
  if (has) {
    warnOnce("stdio streams had content in them that was not flushed. you should set EXIT_RUNTIME to 1 (see the Emscripten FAQ), or make sure to emit a newline when you printf etc.");
    warnOnce("(this may also be due to not including full filesystem support - try building with -sFORCE_FILESYSTEM)");
  }
}

var wasmExports;

if ((!(ENVIRONMENT_IS_PTHREAD))) {
  // Call createWasm on startup if we are the main thread.
  // Worker threads call this once they receive the module via postMessage
  // In modularize mode the generated code is within a factory function so we
  // can use await here (since it's not top-level-await).
  wasmExports = await createWasm();
  await run();
}

// end include: postamble.js
// include: postamble_modularize.js
// In MODULARIZE mode we wrap the generated code in a factory function
// and return either the Module itself, or a promise of the module.
// Assertion for attempting to access module properties on the incoming
// moduleArg.  In the past we used this object as the prototype of the module
// and assigned properties to it, but now we return a distinct object.  This
// keeps the instance private until it is ready (i.e the promise has been
// resolved).
for (const prop of Object.keys(Module)) {
  if (!(prop in moduleArg)) {
    Object.defineProperty(moduleArg, prop, {
      configurable: true,
      get() {
        abort(`Access to module property ('${prop}') is no longer possible via the module constructor argument; Instead, use the result of the module constructor.`);
      }
    });
  }
}


  return Module;
}

// Export using a UMD style export, or ES6 exports if selected
export default createLucivy;

// Create code for detecting if we are running in a pthread.
// Normally this detection is done when the module is itself run but
// when running in MODULARIZE mode we need use this to know if we should
// run the module constructor on startup (true only for pthreads).
var isPthread = globalThis.name?.startsWith('em-pthread');
// In order to support both web and node we also need to detect node here.
var isNode = globalThis.process?.versions?.node && globalThis.process?.type != 'renderer';
if (isNode) isPthread = (await import('node:worker_threads')).workerData === 'em-pthread'

isPthread && createLucivy();

