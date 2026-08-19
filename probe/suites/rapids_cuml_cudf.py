# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""RAPIDS: cuDF dataframe plus a cuML random forest over 20M rows.

The broadest suite here. It pulls in three libraries at once (cupy, cudf,
cuml), builds ~1.2 GB of device data, runs groupby aggregations that trigger
internal JIT/PTX compilation, and then trains and scores a forest. Whatever
the boundary does not support tends to surface here first.

Needs: cupy, cudf, cuml.
"""

import cudf
import cuml
import cupy as cp
import time


def memory_pool_crusher(num_rows=20_000_000):
    print(f"Starting RAPIDS suite with {num_rows} rows...")

    start_time = time.perf_counter()

    print("Generating raw GPU data...")
    cp.random.seed(42)
    data = cp.random.rand(num_rows, 4, dtype=cp.float32)
    labels = cp.random.randint(0, 2, size=num_rows, dtype=cp.int32)

    # DataFrame construction triggers internal JIT/PTX compilation.
    print("Building cuDF DataFrame...")
    df = cudf.DataFrame({
        'feature_1': data[:, 0],
        'feature_2': data[:, 1],
        'feature_3': data[:, 2],
        'feature_4': data[:, 3],
        'target': labels
    })

    print("Running groupby & aggregations...")
    agg_df = df.groupby('target').agg({
        'feature_1': ['mean', 'std'],
        'feature_2': ['max', 'min'],
        'feature_3': 'sum'
    })
    print(agg_df)

    # Random forest: complex, irregular device memory access patterns.
    print("Training RandomForestClassifier...")
    rf = cuml.ensemble.RandomForestClassifier(
        n_estimators=50,
        max_depth=10,
        n_bins=128,
        random_state=42
    )

    # Subsample for training, so it finishes in reasonable time.
    train_df = df.sample(n=5_000_000)
    rf.fit(train_df[['feature_1', 'feature_2', 'feature_3', 'feature_4']],
           train_df['target'])

    print("Running inference...")
    predictions = rf.predict(
        df[['feature_1', 'feature_2', 'feature_3', 'feature_4']])

    end_time = time.perf_counter()
    print(f"RAPIDS suite ok, total: {end_time - start_time:.2f}s")
    print(f"prediction distribution:\n{predictions.value_counts()}")


if __name__ == "__main__":
    memory_pool_crusher()
