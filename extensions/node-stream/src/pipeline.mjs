import "./bytes.mjs";
import upstream from "../dist/readable-stream/lib/internal/streams/pipeline.js";

export const { pipeline } = upstream;
export default pipeline;
