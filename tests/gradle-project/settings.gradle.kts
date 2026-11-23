// Regression project for the build cache server.
// Driven by tests/gradle-project/run.sh; everything is configured through
// environment variables so the same checkout serves every scenario.

rootProject.name = "cache-regression"

buildCache {
    // Only the remote cache, so FROM-CACHE can only mean "served by the server".
    local { isEnabled = false }
    remote<HttpBuildCache> {
        url = uri(System.getenv("REMOTE_CACHE_URL") ?: error("REMOTE_CACHE_URL is not set"))
        isPush = System.getenv("REMOTE_CACHE_PUSH") == "true"
        isAllowInsecureProtocol = true
        isUseExpectContinue = System.getenv("REMOTE_CACHE_EXPECT_CONTINUE") == "true"
        credentials {
            username = System.getenv("REMOTE_CACHE_USER")
            password = System.getenv("REMOTE_CACHE_PASSWORD")
        }
    }
}
