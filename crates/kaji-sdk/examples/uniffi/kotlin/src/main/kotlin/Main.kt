import io.github.aaif_kaji.MessageContent
import io.github.aaif_kaji.MessageRole
import io.github.aaif_kaji.ProviderMessage
import io.github.aaif_kaji.ProviderModelConfig
import io.github.aaif_kaji.StreamChunk
import io.github.aaif_kaji.streamFlow
import io.github.aaif_kaji.providers.openai.defaultModel
import io.github.aaif_kaji.providers.openai.provider as openAiProvider
import kotlinx.coroutines.runBlocking

fun main() = runBlocking {
    val apiKey = System.getenv("OPENAI_API_KEY")
    require(!apiKey.isNullOrBlank()) {
        "Set OPENAI_API_KEY before running this example."
    }

    val provider = openAiProvider(apiKey)
    val model = ProviderModelConfig(modelName = defaultModel())
    val messages = listOf(
        ProviderMessage(
            role = MessageRole.USER,
            content = listOf(
                MessageContent.Text(
                    text = "What is the capital of France? Answer in one sentence.",
                ),
            ),
        ),
    )

    provider
        .streamFlow(
            model,
            "You are a knowledgeable geography expert.",
            messages,
        )
        .collect { chunk ->
            when (chunk) {
                is StreamChunk.TextChunk -> print(chunk.text)
                is StreamChunk.EndChunk -> chunk.usage?.let { println("\nusage: $it") }
                is StreamChunk.ErrorChunk -> System.err.println("\nerror: ${chunk.error.message}")
                is StreamChunk.ToolChunk -> Unit
            }
        }
    println()
}
