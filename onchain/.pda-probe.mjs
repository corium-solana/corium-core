import { PublicKey } from "@solana/web3.js";
const SOLDUST = new PublicKey("CoriumcqGZW3cdnAiyWz6jHHveMUmdrw9RC1KXfMsF8S");
const VRF = new PublicKey("Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz");
const IDENTITY = Buffer.from("identity");

// Signer for the REQUEST: PDA([identity], soldust)
const [reqId, reqBump] = PublicKey.findProgramAddressSync([IDENTITY], SOLDUST);
// Signer of the CALLBACK: PDA([identity, soldust], vrf)
const [cbId, cbBump] = PublicKey.findProgramAddressSync([IDENTITY, SOLDUST.toBytes()], VRF);

console.log("request identity (signs our CPI):", reqId.toBase58(), "bump", reqBump);
console.log("callback identity (signs into us):", cbId.toBase58(), "bump", cbBump);
