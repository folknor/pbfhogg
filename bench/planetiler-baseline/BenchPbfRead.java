import com.onthegomap.planetiler.reader.osm.OsmBlockSource;
import com.onthegomap.planetiler.reader.osm.OsmElement;
import com.onthegomap.planetiler.reader.osm.OsmInputFile;

import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.atomic.AtomicLong;

/**
 * PBF read benchmark using Planetiler's OsmInputFile.
 * Counts all nodes, ways, and relations — same workload as pbfhogg's bench_read.
 *
 * Usage: java -cp planetiler.jar:. BenchPbfRead <file.osm.pbf> [runs]
 */
public class BenchPbfRead {

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.err.println("Usage: BenchPbfRead <file.osm.pbf> [runs]");
            System.exit(1);
        }

        Path pbfPath = Path.of(args[0]);
        int runs = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        long fileMb = pbfPath.toFile().length() / 1_000_000;

        // Sequential: single-threaded forEachBlock + decode
        benchSequential(pbfPath, runs, fileMb);

        // Parallel: forEachBlock I/O feeding thread pool decode workers
        benchParallel(pbfPath, runs, fileMb);
    }

    static void benchSequential(Path path, int runs, long fileMb) throws Exception {
        long bestMs = Long.MAX_VALUE;
        long bestNodes = 0, bestWays = 0, bestRelations = 0;

        for (int r = 0; r < runs; r++) {
            long nodes = 0, ways = 0, relations = 0;
            long start = System.nanoTime();

            try (OsmBlockSource source = new OsmInputFile(path).get()) {
                var counter = new long[3]; // nodes, ways, relations
                source.forEachBlock(block -> {
                    for (OsmElement elem : block.decodeElements()) {
                        if (elem instanceof OsmElement.Node) counter[0]++;
                        else if (elem instanceof OsmElement.Way) counter[1]++;
                        else if (elem instanceof OsmElement.Relation) counter[2]++;
                    }
                });
                nodes = counter[0];
                ways = counter[1];
                relations = counter[2];
            }

            long ms = (System.nanoTime() - start) / 1_000_000;
            if (ms < bestMs) {
                bestMs = ms;
                bestNodes = nodes;
                bestWays = ways;
                bestRelations = relations;
            }
        }

        System.err.println("---");
        System.err.println("tool=planetiler");
        System.err.println("mode=sequential");
        System.err.println("elapsed_ms=" + bestMs);
        System.err.println("nodes=" + bestNodes);
        System.err.println("ways=" + bestWays);
        System.err.println("relations=" + bestRelations);
        System.err.println("file_mb=" + fileMb);
    }

    static void benchParallel(Path path, int runs, long fileMb) throws Exception {
        int threads = Runtime.getRuntime().availableProcessors();
        long bestMs = Long.MAX_VALUE;
        long bestNodes = 0, bestWays = 0, bestRelations = 0;

        for (int r = 0; r < runs; r++) {
            ExecutorService pool = Executors.newFixedThreadPool(threads);
            List<Future<long[]>> futures = new ArrayList<>();
            long start = System.nanoTime();

            try (OsmBlockSource source = new OsmInputFile(path).get()) {
                source.forEachBlock(block -> {
                    futures.add(pool.submit(() -> {
                        long[] counts = new long[3];
                        for (OsmElement elem : block.decodeElements()) {
                            if (elem instanceof OsmElement.Node) counts[0]++;
                            else if (elem instanceof OsmElement.Way) counts[1]++;
                            else if (elem instanceof OsmElement.Relation) counts[2]++;
                        }
                        return counts;
                    }));
                });
            }

            long nodes = 0, ways = 0, relations = 0;
            for (Future<long[]> f : futures) {
                long[] counts = f.get();
                nodes += counts[0];
                ways += counts[1];
                relations += counts[2];
            }
            pool.shutdown();

            long ms = (System.nanoTime() - start) / 1_000_000;
            if (ms < bestMs) {
                bestMs = ms;
                bestNodes = nodes;
                bestWays = ways;
                bestRelations = relations;
            }
        }

        System.err.println("---");
        System.err.println("tool=planetiler");
        System.err.println("mode=parallel");
        System.err.println("elapsed_ms=" + bestMs);
        System.err.println("nodes=" + bestNodes);
        System.err.println("ways=" + bestWays);
        System.err.println("relations=" + bestRelations);
        System.err.println("file_mb=" + fileMb);
    }
}
