// RFC 5929's certificate signature algorithm selects the channel-binding digest.
import { digest as sha224 } from "@hibana/sha224";
import { digest as sha256 } from "@hibana/sha256";
import { digest as sha384 } from "@hibana/sha384";
import { digest as sha512 } from "@hibana/sha512";
import { digest as sha512t224 } from "@hibana/sha512-224";
import { digest as sha512t256 } from "@hibana/sha512-256";
export const certificateDigests = Object.freeze({
  "SHA-224": sha224,
  "SHA-256": sha256,
  "SHA-384": sha384,
  "SHA-512": sha512,
  "SHA512-224": sha512t224,
  "SHA512-256": sha512t256,
});
