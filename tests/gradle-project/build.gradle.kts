import java.util.Random

/**
 * Writes `sizeBytes` of deterministic, incompressible data. The cache key
 * depends only on the two inputs, so the output is relocatable between
 * checkouts.
 */
@CacheableTask
abstract class Generate : DefaultTask() {
    @get:Input
    abstract val seed: Property<String>

    @get:Input
    abstract val sizeBytes: Property<Int>

    @get:OutputFile
    abstract val output: RegularFileProperty

    @TaskAction
    fun generate() {
        val bytes = ByteArray(sizeBytes.get())
        Random(seed.get().hashCode().toLong()).nextBytes(bytes)
        output.get().asFile.writeBytes(bytes)
    }
}

// Distinguishes runs, so each scenario works with keys the server has not seen.
val variant = providers.gradleProperty("variant").orElse("default")

fun generate(name: String, size: Int) = tasks.register<Generate>(name) {
    seed.set(variant.map { "$name-$it" })
    sizeBytes.set(size)
    output.set(layout.buildDirectory.file("generated/$name.bin"))
}

// Four ordinary cacheable tasks.
val small = listOf(
    generate("generateA", 2_000),
    generate("generateB", 50_000),
    generate("generateC", 300_000),
    generate("generateD", 900_000),
)
tasks.register("generateAll") { dependsOn(small) }

// An output larger than the server's artifact limit in the test (1 MiB): the
// server discards it but must answer in a way that keeps the cache enabled.
val big = generate("generateBig", 3_000_000)

// Runs after the oversized upload. If that upload had disabled the remote
// cache, this output would never be stored.
val afterBig = generate("generateAfterBig", 10_000)
afterBig.configure { mustRunAfter(big) }
