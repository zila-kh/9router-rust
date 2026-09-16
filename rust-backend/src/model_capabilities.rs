//! Model capability tables, ported from upstream
//! `open-sse/providers/capabilities.js` — the module the dashboard and
//! `GET /v1/models` both consult.
//!
//! The tables below are the upstream data verbatim, kept as JSON so a future
//! upstream sync can be diffed mechanically. Lookup order matches
//! `getCapabilitiesForModel`: provider override -> canonical exact id ->
//! ordered glob patterns -> `DEFAULT_CAPABILITIES` floor, with the name-based
//! vision heuristic (`open-sse/providers/visionPatterns.js`) applied last.
//!
//! Upstream also folds in a models.dev catalog snapshot through
//! `setCatalogSource`; the Rust backend has no synced catalog reader, so that
//! strictly additive layer is not reproduced (see docs/PARITY.md).

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{Map, Value};

const DEFAULT_CAPABILITIES_JSON: &str = r##"{
  "vision": false,
  "pdf": false,
  "audioInput": false,
  "videoInput": false,
  "imageOutput": false,
  "audioOutput": false,
  "search": false,
  "tools": true,
  "reasoning": false,
  "thinkingFormat": null,
  "thinkingCanDisable": true,
  "thinkingRange": null,
  "thinkingEffortSupported": false,
  "contextWindow": 200000,
  "maxOutput": 64000
}"##;

const SERVICE_KIND_CAPABILITIES_JSON: &str = r##"{
  "imageToText": {
    "vision": true
  },
  "image": {
    "imageOutput": true
  },
  "stt": {
    "audioInput": true
  },
  "tts": {
    "audioOutput": true
  },
  "embedding": {
    "tools": false
  }
}"##;

const MODEL_CAPABILITIES_JSON: &str = r##"{
  "claude-fable-5-1": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "thinkingCanDisable": false,
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-5": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-5-thinking": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-5-agentic": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-5-thinking-agentic": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4.6": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4.7": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4-7": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4.8": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4-6": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4-8": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4.8-thinking": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-opus-4-8-thinking": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-sonnet-4.6": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-sonnet-4-6": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-sonnet-5": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-sonnet-5-thinking": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-sonnet-5-agentic": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "claude-sonnet-5-thinking-agentic": {
    "vision": true,
    "reasoning": true,
    "search": true,
    "thinkingFormat": "claude-adaptive",
    "contextWindow": 1000000,
    "maxOutput": 128000
  },
  "gpt-image-1": {
    "imageOutput": true,
    "tools": false
  },
  "glm-5.3-flash": {
    "vision": true,
    "videoInput": true,
    "pdf": true,
    "reasoning": true,
    "thinkingFormat": "zai",
    "contextWindow": 1000000,
    "maxOutput": 131072
  },
  "glm-4.6v": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "zai",
    "contextWindow": 128000,
    "maxOutput": 32768
  },
  "glm-4.5v": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "zai",
    "contextWindow": 64000,
    "maxOutput": 16384
  },
  "deepseek-v4-flash-vision-exp": {
    "vision": true,
    "reasoning": true,
    "thinkingFormat": "deepseek",
    "contextWindow": 1000000,
    "maxOutput": 384000
  },
  "vision-model": {
    "vision": true,
    "reasoning": true,
    "thinkingFormat": "qwen",
    "contextWindow": 1000000
  },
  "coder-model": {
    "reasoning": true,
    "thinkingFormat": "qwen",
    "contextWindow": 1000000
  },
  "kimi-k3": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "kimi",
    "thinkingCanDisable": false,
    "contextWindow": 1048576,
    "maxOutput": 131072
  },
  "k3": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "kimi",
    "thinkingCanDisable": false,
    "contextWindow": 1048576,
    "maxOutput": 131072
  },
  "kimi-for-coding": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "kimi",
    "thinkingCanDisable": false,
    "contextWindow": 262144,
    "maxOutput": 65536
  },
  "kimi-for-coding-highspeed": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "kimi",
    "thinkingCanDisable": false,
    "contextWindow": 262144,
    "maxOutput": 65536
  },
  "kimi-k2.7-code": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "kimi",
    "thinkingCanDisable": false,
    "contextWindow": 262144,
    "maxOutput": 65536
  },
  "kimi-k2.7-code-highspeed": {
    "vision": true,
    "videoInput": true,
    "reasoning": true,
    "thinkingFormat": "kimi",
    "thinkingCanDisable": false,
    "contextWindow": 262144,
    "maxOutput": 65536
  },
  "muse-spark-1.2-contributor-free": {
    "vision": true,
    "reasoning": true,
    "thinkingFormat": "openai",
    "contextWindow": 1048576,
    "maxOutput": 131072
  },
  "muse-spark-1.3-contributor-free": {
    "vision": true,
    "reasoning": true,
    "thinkingFormat": "openai",
    "contextWindow": 1048576,
    "maxOutput": 131072
  }
}"##;

const PROVIDER_CAPABILITIES_JSON: &str = r##"{
  "nvidia": {
    "minimaxai/minimax-m2.7": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 200000,
      "maxOutput": 131072
    },
    "minimaxai/minimax-m3": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 512000,
      "maxOutput": 131072
    },
    "z-ai/glm-5.2": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 128000
    },
    "deepseek-ai/deepseek-v4-pro": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 1000000,
      "maxOutput": 65536
    },
    "deepseek-ai/deepseek-v4-flash": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 1000000,
      "maxOutput": 65536
    }
  },
  "codex": {
    "gpt-6-astra": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-sol": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 372000,
      "maxOutput": 128000
    },
    "gpt-5.6-sol-review": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 372000,
      "maxOutput": 128000
    },
    "gpt-5.6-terra": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-terra-review": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-luna": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-luna-review": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    }
  },
  "kiro": {
    "gpt-5.6-sol": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-terra": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-luna": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-sol-thinking": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-terra-thinking": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-luna-thinking": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-sol-agentic": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-terra-agentic": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-luna-agentic": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-sol-thinking-agentic": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-terra-thinking-agentic": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    },
    "gpt-5.6-luna-thinking-agentic": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    }
  },
  "codebuddy-cn": {
    "glm-5.2": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": true,
      "contextWindow": 1000000,
      "maxOutput": 48000
    },
    "glm-5.1": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 200000,
      "maxOutput": 48000
    },
    "glm-5.0": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 48000
    },
    "glm-5v-turbo": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 200000,
      "maxOutput": 64000
    },
    "glm-4.7": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 48000
    },
    "minimax-m3": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 512000,
      "maxOutput": 128000
    },
    "kimi-k2.7": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 256000,
      "maxOutput": 32000
    },
    "kimi-k2.6": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 256000,
      "maxOutput": 32000
    },
    "hy3": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 192000,
      "maxOutput": 64000
    },
    "hy4-preview": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 64000
    },
    "glm-5.3": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": true,
      "contextWindow": 1000000,
      "maxOutput": 48000
    },
    "glm-5.3-flash": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": true,
      "contextWindow": 1000000,
      "maxOutput": 32000
    },
    "kimi-k3-1": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 32000
    },
    "deepseek-v4-pro": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": true,
      "contextWindow": 1000000,
      "maxOutput": 50000
    },
    "deepseek-v4.1-flash": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "thinkingCanDisable": true,
      "contextWindow": 1000000,
      "maxOutput": 128000
    }
  },
  "qoder": {
    "ultimate": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "claude-adaptive",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 128000
    },
    "performance": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "claude-adaptive",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 128000
    },
    "dmodel": {
      "reasoning": true,
      "thinkingFormat": "deepseek",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 65536
    },
    "dfmodel": {
      "reasoning": true,
      "thinkingFormat": "deepseek",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 65536
    },
    "gmodel": {
      "reasoning": true,
      "thinkingFormat": "zai",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 128000
    },
    "gfmodel": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "zai",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 128000
    },
    "kmodel_latest": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "kimi",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 65536
    },
    "kmodel": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "kimi",
      "thinkingCanDisable": false,
      "contextWindow": 256000,
      "maxOutput": 65536
    },
    "mmodel": {
      "reasoning": true,
      "thinkingFormat": "minimax",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 512000
    },
    "qmodel_latest": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 65536
    },
    "qmodel": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 65536
    },
    "qfmodel": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 65536
    },
    "qmodel_38max": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "thinkingCanDisable": false,
      "contextWindow": 1000000,
      "maxOutput": 65536
    }
  },
  "poolside": {
    "laguna-s-2.1": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 1000000,
      "maxOutput": 32000
    },
    "laguna-xs-2.1": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 32000
    }
  }
}"##;

const PATTERN_CAPABILITIES_JSON: &str = r##"[
  {
    "pattern": "*claude*opus-5*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-adaptive",
      "contextWindow": 1000000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*claude*opus-4.6*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-adaptive"
    }
  },
  {
    "pattern": "*claude*opus-4.7*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-adaptive"
    }
  },
  {
    "pattern": "*claude*opus-4.8*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-adaptive"
    }
  },
  {
    "pattern": "*claude*sonnet-4.6*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-adaptive"
    }
  },
  {
    "pattern": "*claude*sonnet-4.7*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-adaptive"
    }
  },
  {
    "pattern": "*claude*haiku*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-budget"
    }
  },
  {
    "pattern": "*claude*opus*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-budget"
    }
  },
  {
    "pattern": "*claude*sonnet*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-budget"
    }
  },
  {
    "pattern": "*claude*fable*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-budget",
      "contextWindow": 1000000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*claude*mythos*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-budget",
      "contextWindow": 1000000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*claude-3*",
    "caps": {
      "vision": true
    }
  },
  {
    "pattern": "*claude*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "claude-budget"
    }
  },
  {
    "pattern": "*gemini*image*",
    "caps": {
      "vision": true,
      "imageOutput": true,
      "contextWindow": 1048576
    }
  },
  {
    "pattern": "*gemini-3.8*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "gemini-level",
      "thinkingCanDisable": false,
      "contextWindow": 1048576,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*gemini-3.7*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "gemini-level",
      "thinkingCanDisable": false,
      "contextWindow": 1048576,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*gemini-3*pro*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "gemini-level",
      "thinkingCanDisable": false,
      "contextWindow": 1048576,
      "maxOutput": 65535
    }
  },
  {
    "pattern": "*gemini-3*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "gemini-level",
      "thinkingCanDisable": false,
      "contextWindow": 1048576,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*gemini-2.5*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "gemini-budget",
      "thinkingRange": {
        "min": 0,
        "max": 24576
      },
      "contextWindow": 1048576,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*gemini-2*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "search": true,
      "contextWindow": 1048576,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*gemini*",
    "caps": {
      "vision": true,
      "search": true,
      "contextWindow": 1048576
    }
  },
  {
    "pattern": "*gemma*",
    "caps": {
      "vision": true,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*nanobanana*",
    "caps": {
      "vision": true,
      "imageOutput": true
    }
  },
  {
    "pattern": "*gpt-6*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 272000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*gpt-5*image*",
    "caps": {
      "imageOutput": true
    }
  },
  {
    "pattern": "*gpt-5*codex*",
    "caps": {
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 400000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*gpt-5*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 400000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*gpt-4o*",
    "caps": {
      "vision": true,
      "search": true,
      "contextWindow": 128000,
      "maxOutput": 16384
    }
  },
  {
    "pattern": "*gpt-4.1*",
    "caps": {
      "vision": true,
      "contextWindow": 1000000,
      "maxOutput": 32768
    }
  },
  {
    "pattern": "*gpt-4-turbo*",
    "caps": {
      "vision": true,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*gpt-4*",
    "caps": {
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*gpt-3.5*",
    "caps": {
      "contextWindow": 16385,
      "maxOutput": 4096
    }
  },
  {
    "pattern": "*gpt-oss*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*o1-mini*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*o1*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 100000
    }
  },
  {
    "pattern": "*o3*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 100000
    }
  },
  {
    "pattern": "*o4*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 100000
    }
  },
  {
    "pattern": "*grok*image*",
    "caps": {
      "imageOutput": true
    }
  },
  {
    "pattern": "*grok-code*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 256000
    }
  },
  {
    "pattern": "*grok-4.6*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 500000,
      "maxOutput": 500000
    }
  },
  {
    "pattern": "*grok-4.5*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 500000,
      "maxOutput": 64000
    }
  },
  {
    "pattern": "*grok-4*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 256000
    }
  },
  {
    "pattern": "*grok-3*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 131072
    }
  },
  {
    "pattern": "*grok*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "search": true,
      "thinkingFormat": "openai",
      "contextWindow": 256000
    }
  },
  {
    "pattern": "*qwen*vl*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 262144
    }
  },
  {
    "pattern": "*qwen*omni*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 262144,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*qwen*coder*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 1000000
    }
  },
  {
    "pattern": "*qwen*max*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 1000000,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*qwen3.5*",
    "caps": {
      "vision": true,
      "videoInput": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 1000000,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*qwen3.6*",
    "caps": {
      "vision": true,
      "videoInput": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 1000000,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*qwen3.7*",
    "caps": {
      "vision": true,
      "videoInput": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 1000000,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*qwen*plus*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 1000000,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*qwen*235b*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 262144
    }
  },
  {
    "pattern": "*qwq*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "qwen",
      "thinkingCanDisable": false,
      "contextWindow": 131072
    }
  },
  {
    "pattern": "*qwen*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "qwen",
      "contextWindow": 262144
    }
  },
  {
    "pattern": "*kimi*k3*",
    "caps": {
      "vision": true,
      "videoInput": true,
      "reasoning": true,
      "thinkingFormat": "kimi",
      "thinkingCanDisable": false,
      "contextWindow": 1048576,
      "maxOutput": 131072
    }
  },
  {
    "pattern": "*kimi*for-coding*",
    "caps": {
      "vision": true,
      "videoInput": true,
      "reasoning": true,
      "thinkingFormat": "kimi",
      "thinkingCanDisable": false,
      "contextWindow": 262144,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*kimi*k2.7*code*",
    "caps": {
      "vision": true,
      "videoInput": true,
      "reasoning": true,
      "thinkingFormat": "kimi",
      "thinkingCanDisable": false,
      "contextWindow": 262144,
      "maxOutput": 65536
    }
  },
  {
    "pattern": "*kimi*k2*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "kimi",
      "contextWindow": 262144,
      "maxOutput": 262144
    }
  },
  {
    "pattern": "*kimi*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "kimi",
      "contextWindow": 262144
    }
  },
  {
    "pattern": "*glm-5.3*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "zai",
      "thinkingEffortSupported": true,
      "contextWindow": 200000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*glm-5.2*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "zai",
      "thinkingEffortSupported": true,
      "contextWindow": 200000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*glm-5*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "zai",
      "contextWindow": 200000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*glm-4.7*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "zai",
      "contextWindow": 200000,
      "maxOutput": 128000
    }
  },
  {
    "pattern": "*glm-4*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "zai",
      "contextWindow": 200000
    }
  },
  {
    "pattern": "*glm*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "zai",
      "contextWindow": 200000
    }
  },
  {
    "pattern": "*deepseek-v4*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "deepseek",
      "contextWindow": 1000000,
      "maxOutput": 384000
    }
  },
  {
    "pattern": "*reasoner*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "deepseek",
      "thinkingCanDisable": false,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*deepseek-r*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "deepseek",
      "thinkingCanDisable": false,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*deepseek-chat*",
    "caps": {
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*deepseek*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "deepseek",
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*minimax*image*",
    "caps": {
      "imageOutput": true
    }
  },
  {
    "pattern": "*minimax-m3*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "minimax",
      "contextWindow": 1048576,
      "maxOutput": 512000
    }
  },
  {
    "pattern": "*minimax-m2.7*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "minimax",
      "thinkingCanDisable": false,
      "contextWindow": 204800,
      "maxOutput": 131072
    }
  },
  {
    "pattern": "*minimax*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "minimax",
      "thinkingCanDisable": false,
      "contextWindow": 200000,
      "maxOutput": 131072
    }
  },
  {
    "pattern": "*mimo*v2.5*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "videoInput": true,
      "contextWindow": 1048576,
      "maxOutput": 131072
    }
  },
  {
    "pattern": "*mimo*omni*",
    "caps": {
      "vision": true,
      "audioInput": true,
      "contextWindow": 262144,
      "maxOutput": 131072
    }
  },
  {
    "pattern": "*mimo*",
    "caps": {
      "vision": true,
      "contextWindow": 262144,
      "maxOutput": 131072
    }
  },
  {
    "pattern": "*llama-4*",
    "caps": {
      "vision": true,
      "contextWindow": 1000000
    }
  },
  {
    "pattern": "*llama*",
    "caps": {
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*codestral*",
    "caps": {
      "contextWindow": 256000
    }
  },
  {
    "pattern": "*mistral-large*",
    "caps": {
      "vision": true,
      "contextWindow": 256000
    }
  },
  {
    "pattern": "*mistral*",
    "caps": {
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*command-a-vision*",
    "caps": {
      "vision": true,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*command*",
    "caps": {
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*sonar*",
    "caps": {
      "search": true,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*pplx*",
    "caps": {
      "search": true,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*perplexity*",
    "caps": {
      "search": true,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*laguna-s-2.1*free*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 32000
    }
  },
  {
    "pattern": "*laguna-s-2.1*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 1000000,
      "maxOutput": 32000
    }
  },
  {
    "pattern": "*laguna*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 200000,
      "maxOutput": 32000
    }
  },
  {
    "pattern": "*muse*spark*",
    "caps": {
      "vision": true,
      "reasoning": true,
      "thinkingFormat": "openai",
      "contextWindow": 1048576,
      "maxOutput": 131072
    }
  },
  {
    "pattern": "*hunyuan*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "hunyuan",
      "contextWindow": 262144,
      "maxOutput": 262144
    }
  },
  {
    "pattern": "hy3*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "hunyuan",
      "contextWindow": 262144,
      "maxOutput": 262144
    }
  },
  {
    "pattern": "*step-*",
    "caps": {
      "reasoning": true,
      "thinkingFormat": "step",
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*nemotron*",
    "caps": {
      "reasoning": true,
      "contextWindow": 128000
    }
  },
  {
    "pattern": "*ling-*",
    "caps": {
      "reasoning": true,
      "contextWindow": 128000
    }
  }
]"##;

fn parse(json: &str) -> Value {
    serde_json::from_str(json).expect("capability table is valid JSON")
}

static DEFAULT_CAPABILITIES: Lazy<Value> = Lazy::new(|| parse(DEFAULT_CAPABILITIES_JSON));
static SERVICE_KIND_CAPABILITIES: Lazy<Value> = Lazy::new(|| parse(SERVICE_KIND_CAPABILITIES_JSON));
static MODEL_CAPABILITIES: Lazy<Value> = Lazy::new(|| parse(MODEL_CAPABILITIES_JSON));
static PROVIDER_CAPABILITIES: Lazy<Value> = Lazy::new(|| parse(PROVIDER_CAPABILITIES_JSON));

/// `PATTERN_CAPABILITIES` with every glob pre-compiled once: the list is
/// consulted for every model in the `/v1/models` payload.
static PATTERN_CAPABILITY_REGEXES: Lazy<Vec<(Regex, Value)>> = Lazy::new(|| {
    let table = parse(PATTERN_CAPABILITIES_JSON);
    table
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let pattern = entry.get("pattern").and_then(Value::as_str)?;
                    let caps = entry.get("caps")?.clone();
                    Some((glob_regex(pattern), caps))
                })
                .collect()
        })
        .unwrap_or_default()
});

/// `DEFAULT_CAPABILITIES` — the safe floor every result is merged over.
pub fn default_capabilities() -> Value {
    DEFAULT_CAPABILITIES.clone()
}

/// `capabilitiesFromServiceKind(kind)` — the delta object for a dashboard
/// service kind, or `None` when the kind has no capability mapping (upstream
/// returns `null` for e.g. `"llm"`).
pub fn capabilities_from_service_kind(kind: Option<&str>) -> Option<Value> {
    let kind = kind?;
    SERVICE_KIND_CAPABILITIES
        .as_object()
        .and_then(|map| map.get(kind))
        .cloned()
}

fn merge(base: &Value, delta: &Value) -> Value {
    let mut out: Map<String, Value> = base.as_object().cloned().unwrap_or_default();
    if let Some(delta) = delta.as_object() {
        for (key, value) in delta {
            out.insert(key.clone(), value.clone());
        }
    }
    Value::Object(out)
}

/// `matchPattern(pattern, model)` from `open-sse/providers/pricing.js`:
/// glob with `*` wildcards, anchored, case-insensitive.
fn glob_regex(pattern: &str) -> Regex {
    let mut source = String::from("(?i)^");
    for (index, part) in pattern.split('*').enumerate() {
        if index > 0 {
            source.push_str(".*");
        }
        source.push_str(&regex::escape(part));
    }
    source.push('$');
    Regex::new(&source).expect("capability glob pattern compiles")
}

fn match_pattern(pattern: &str, model: &str) -> bool {
    glob_regex(pattern).is_match(model)
}

/// Name-based vision detection, ported from
/// `open-sse/providers/visionPatterns.js` (`looksLikeVisionModel`).
pub fn looks_like_vision_model(model_id: &str) -> bool {
    if model_id.is_empty() {
        return false;
    }
    let id = model_id.to_lowercase();
    if NOT_VISION.is_match(&id) {
        return false;
    }
    VISION_NAME.is_match(&id)
}

const SEP: &str = "[-_/:.]";

static NOT_VISION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        "(?i)(^|{SEP})(image|img)({SEP}|$)|stable-image|gen[0-9]_image|nanobanana|imagine|t2v|i2v|flux|dall|sdxl|diffusion|embed|rerank|guard|moderation|tts|stt|whisper|voice|speech|audio"
    ))
    .expect("NOT_VISION regex")
});

static VISION_NAME: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        "(?i)(^|{SEP})(vision|vl|vlm|multimodal|omni|visual)({SEP}|$)|[0-9]\\.[0-9]+v({SEP}|$)|(^|{SEP})glm-[0-9]+v({SEP}|$)|(^|{SEP})(llava|pixtral|internvl|cogvlm|minicpm-v|moondream|idefics|fuyu)"
    ))
    .expect("VISION_NAME regex")
});

/// `refine(base, provider, model)` — merge over the floor and apply the
/// name-based vision heuristic. The models.dev catalog layer is intentionally
/// absent (no native catalog reader).
fn refine(base: Option<&Value>, model: &str) -> Value {
    let mut result = match base {
        Some(base) => merge(&DEFAULT_CAPABILITIES, base),
        None => default_capabilities(),
    };
    let vision = result
        .get("vision")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !vision && looks_like_vision_model(model) {
        result["vision"] = Value::Bool(true);
    }
    result
}

/// `getCapabilitiesForModel(provider, model)` — the full fallback chain.
pub fn get_capabilities_for_model(provider: Option<&str>, model: &str) -> Value {
    if model.is_empty() {
        return default_capabilities();
    }

    let base_model = model.rsplit('/').next().unwrap_or(model);

    if let Some(provider) = provider.filter(|value| !value.is_empty()) {
        if let Some(provider_caps) = PROVIDER_CAPABILITIES.get(provider) {
            if let Some(delta) = provider_caps.get(model) {
                return merge(&DEFAULT_CAPABILITIES, delta);
            }
            if let Some(delta) = provider_caps.get(base_model) {
                return merge(&DEFAULT_CAPABILITIES, delta);
            }
        }
    }

    if let Some(delta) = MODEL_CAPABILITIES.get(base_model) {
        return merge(&DEFAULT_CAPABILITIES, delta);
    }
    if let Some(delta) = MODEL_CAPABILITIES.get(model) {
        return merge(&DEFAULT_CAPABILITIES, delta);
    }

    for (pattern, caps) in PATTERN_CAPABILITY_REGEXES.iter() {
        if pattern.is_match(base_model) || pattern.is_match(model) {
            return refine(Some(caps), model);
        }
    }

    refine(None, model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_kind_deltas_match_upstream() {
        assert_eq!(
            capabilities_from_service_kind(Some("imageToText")),
            Some(serde_json::json!({"vision": true}))
        );
        // upstream returns null for the LLM sentinel
        assert!(capabilities_from_service_kind(Some("llm")).is_none());
        assert!(capabilities_from_service_kind(None).is_none());
    }

    #[test]
    fn defaults_are_the_safe_floor() {
        let caps = get_capabilities_for_model(None, "totally-unknown-model-xyz");
        assert_eq!(caps["contextWindow"], 200_000);
        assert_eq!(caps["maxOutput"], 64_000);
        assert_eq!(caps["tools"], true);
        assert_eq!(caps["thinkingFormat"], Value::Null);
    }

    #[test]
    fn exact_model_capabilities_win_over_patterns() {
        let caps = get_capabilities_for_model(Some("anthropic"), "claude-opus-4.7");
        assert_eq!(caps["contextWindow"], 1_000_000);
        assert_eq!(caps["maxOutput"], 128_000);
        assert_eq!(caps["search"], true);
    }

    #[test]
    fn vision_heuristics_only_turn_vision_on() {
        assert!(looks_like_vision_model("qwen3-vl-plus"));
        assert!(looks_like_vision_model("glm-4.6v"));
        assert!(!looks_like_vision_model("gpt-4v"));
        assert!(!looks_like_vision_model("dall-e-3"));
        assert!(!looks_like_vision_model("text-embedding-3-large"));
    }

    #[test]
    fn glob_patterns_are_anchored_and_case_insensitive() {
        assert!(match_pattern("claude-*", "claude-opus-4.7"));
        assert!(!match_pattern("claude-*", "anthropic/claude"));
        assert!(match_pattern("minimax-*", "MiniMax-M2.5"));
    }
}
